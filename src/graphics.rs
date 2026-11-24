//! A VM's graphical display: which graphics devices a new VM gets, how `set_display` replaces them in a persistent
//! definition, and where a running VM's VNC server listens. The functions are pure; `manage` does the libvirt calls.

use anyhow::{Result, bail};
use schemars::JsonSchema;
use serde::Deserialize;

/// The loopback address a VNC display listens on.
const LOOPBACK: &str = "127.0.0.1";

/// Which displays a VM has.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum DisplayKind {
    /// SPICE only, the default.
    #[default]
    Spice,
    /// VNC only, on 127.0.0.1 with an automatic port and no password.
    Vnc,
    /// Both, so a SPICE client and a VNC client can connect.
    Both,
}

const SPICE_XML: &str = "<graphics type='spice' autoport='yes'><listen type='address'/>\
                         <image compression='off'/></graphics>";
const VNC_XML: &str = "<graphics type='vnc' autoport='yes' listen='127.0.0.1'>\
                       <listen type='address' address='127.0.0.1'/></graphics>";

impl DisplayKind {
    /// The `<graphics>` elements of a new VM with this display.
    pub fn graphics_xml(self) -> String {
        match self {
            DisplayKind::Spice => SPICE_XML.to_string(),
            DisplayKind::Vnc => VNC_XML.to_string(),
            DisplayKind::Both => format!("{SPICE_XML}{VNC_XML}"),
        }
    }

    /// The word the tools use for this display.
    pub fn word(self) -> &'static str {
        match self {
            DisplayKind::Spice => "spice",
            DisplayKind::Vnc => "vnc",
            DisplayKind::Both => "both",
        }
    }

    fn has_spice(self) -> bool {
        self != DisplayKind::Vnc
    }
}

/// The `<graphics>` children of a domain's `<devices>`.
fn graphics_of<'a, 'i>(
    devices: roxmltree::Node<'a, 'i>,
) -> impl Iterator<Item = roxmltree::Node<'a, 'i>> {
    devices.children().filter(|n| n.has_tag_name("graphics"))
}

/// Whether `address` is a loopback address.
fn is_loopback(address: &str) -> bool {
    matches!(address, "127.0.0.1" | "::1" | "localhost")
}

/// Why `set_display` may not replace this display, or `None` when it is exactly what `create_vm` writes and libvirt
/// adds back: the attributes `type`, `autoport` (`yes`), `port` and `tlsPort` (`-1`) and `listen` (loopback), a
/// `<listen type='address'>` child on loopback, and `<image compression='off'/>`. Any other setting would be lost
/// when the element is replaced, so the reason names it. It never repeats a value, because the definition may hold a
/// password.
fn unreplaceable(g: roxmltree::Node<'_, '_>) -> Option<String> {
    let kind = g.attribute("type").unwrap_or_default();
    if !matches!(kind, "spice" | "vnc") {
        return Some(format!("a {kind} display"));
    }
    for attr in g.attributes() {
        let (name, value) = (attr.name(), attr.value());
        let refused = match name {
            "type" => None,
            "passwd" | "passwdValidTo" => Some(format!("a {kind} display with a password")),
            "autoport" => (value != "yes").then(|| with_setting(kind, name)),
            "port" | "tlsPort" => {
                (value != "-1").then(|| format!("a {kind} display with a fixed port"))
            }
            "listen" => (!is_loopback(value)).then(|| off_loopback(kind)),
            "socket" => Some(format!("a {kind} display on a socket")),
            _ => Some(with_setting(kind, name)),
        };
        if refused.is_some() {
            return refused;
        }
    }
    for child in g.children().filter(roxmltree::Node::is_element) {
        let tag = child.tag_name().name();
        let refused = match tag {
            "listen" => listener_refusal(kind, child),
            "image" => child
                .attributes()
                .find(|a| (a.name(), a.value()) != ("compression", "off"))
                .map(|a| with_setting(kind, a.name()))
                .or_else(|| has_element(child).then(|| with_setting(kind, tag))),
            _ => Some(with_setting(kind, tag)),
        };
        if refused.is_some() {
            return refused;
        }
    }
    None
}

/// Why a `<listen>` child of a `kind` display is not the loopback address listener the server writes, or `None`.
fn listener_refusal(kind: &str, listen: roxmltree::Node<'_, '_>) -> Option<String> {
    if listen.attribute("type") != Some("address") {
        return Some(off_loopback(kind));
    }
    for attr in listen.attributes() {
        match (attr.name(), attr.value()) {
            ("type", _) => {}
            ("address", address) if is_loopback(address) => {}
            ("address", _) => return Some(off_loopback(kind)),
            (name, _) => return Some(with_setting(kind, name)),
        }
    }
    has_element(listen).then(|| with_setting(kind, "listen"))
}

fn with_setting(kind: &str, name: &str) -> String {
    format!("a {kind} display with the `{name}` setting")
}

fn off_loopback(kind: &str) -> String {
    format!("a {kind} display that does not listen on the loopback address")
}

fn has_element(node: roxmltree::Node<'_, '_>) -> bool {
    node.children().any(|n| n.is_element())
}

/// `xml`, a domain definition, with its `<graphics>` elements replaced by those of `kind`, and any SPICE audio backend
/// turned off when the new display has no SPICE, because libvirt refuses `<audio type='spice'/>` without SPICE
/// graphics. Everything else stays byte for byte.
///
/// # Errors
///
/// Fails when the XML does not parse or has no `<devices>`, or when an existing display is more than the server
/// writes: another type than SPICE or VNC, a password, a fixed port, a listener other than loopback, or any other
/// setting, such as a SPICE clipboard rule or a keymap. Replacing such a display would drop what an operator set up.
/// It also fails when the new display has no SPICE and the VM has a `spicevmc` channel or redirected USB device,
/// which libvirt refuses without SPICE graphics.
pub fn with_display(xml: &str, kind: DisplayKind) -> Result<String> {
    let doc = roxmltree::Document::parse(xml)?;
    let Some(devices) = doc
        .root_element()
        .children()
        .find(|n| n.has_tag_name("devices"))
    else {
        bail!("the definition has no <devices>");
    };
    if !kind.has_spice()
        && let Some(device) = devices.children().find(|n| {
            (n.has_tag_name("channel") || n.has_tag_name("redirdev"))
                && n.attribute("type") == Some("spicevmc")
        })
    {
        bail!(
            "set_display cannot remove SPICE from this VM: it has a spicevmc <{}> device, and libvirt refuses that \
             device without SPICE graphics",
            device.tag_name().name()
        );
    }
    // Each edit replaces the byte range with the text.
    let mut edits: Vec<(std::ops::Range<usize>, String)> = Vec::new();
    for g in graphics_of(devices) {
        if let Some(why) = unreplaceable(g) {
            bail!(
                "set_display replaces only the SPICE and VNC displays that create_vm writes; this VM has {why}"
            );
        }
        let new = if edits.is_empty() {
            kind.graphics_xml()
        } else {
            String::new()
        };
        edits.push((g.range(), new));
    }
    if edits.is_empty() {
        let end = devices.range().end;
        let close = end.checked_sub("</devices>".len());
        // `get` refuses a range that cuts a multibyte character, which a self-closing element can produce.
        let Some(close) = close.filter(|&c| xml.get(c..end) == Some("</devices>")) else {
            bail!("the definition's <devices> has no closing tag");
        };
        edits.push((close..close, kind.graphics_xml()));
    }
    if !kind.has_spice() {
        for audio in devices
            .children()
            .filter(|n| n.has_tag_name("audio") && n.attribute("type") == Some("spice"))
        {
            let id = audio
                .attribute("id")
                .map(|id| format!(" id='{id}'"))
                .unwrap_or_default();
            edits.push((audio.range(), format!("<audio{id} type='none'/>")));
        }
    }
    edits.sort_by_key(|(range, _)| range.start);
    let mut out = String::with_capacity(xml.len());
    let mut pos = 0;
    for (range, new) in edits {
        out.push_str(&xml[pos..range.start]);
        out.push_str(&new);
        pos = range.end;
    }
    out.push_str(&xml[pos..]);
    Ok(out)
}

/// The types of the `<graphics>` elements of a domain definition, in order.
///
/// # Errors
///
/// Fails when the XML does not parse.
pub fn displays(xml: &str) -> Result<Vec<String>> {
    let doc = roxmltree::Document::parse(xml)?;
    Ok(doc
        .root_element()
        .children()
        .filter(|n| n.has_tag_name("devices"))
        .flat_map(graphics_of)
        .map(|g| g.attribute("type").unwrap_or_default().to_string())
        .collect())
}

/// The address and port of the first VNC display in the live definition of a running VM, or `None` when it has none
/// or no port is assigned yet.
pub fn vnc_endpoint(live_xml: &str) -> Option<(String, u16)> {
    let doc = roxmltree::Document::parse(live_xml).ok()?;
    let devices = doc
        .root_element()
        .children()
        .find(|n| n.has_tag_name("devices"))?;
    let g = graphics_of(devices).find(|g| g.attribute("type") == Some("vnc"))?;
    let port: u16 = g.attribute("port")?.parse().ok().filter(|p| *p > 0)?;
    let address = g
        .children()
        .find(|n| n.has_tag_name("listen"))
        .and_then(|n| n.attribute("address"))
        .or(g.attribute("listen"))
        .unwrap_or(LOOPBACK);
    Some((address.to_string(), port))
}

#[cfg(test)]
mod tests {
    use super::*;

    const VNC: &str = "<graphics type='vnc' autoport='yes' listen='127.0.0.1'>\
                       <listen type='address' address='127.0.0.1'/></graphics>";
    const SPICE: &str = "<graphics type='spice' autoport='yes'><listen type='address'/>\
                         <image compression='off'/></graphics>";

    fn domain(devices: &str) -> String {
        format!(
            "<domain type='kvm'><name>t</name><metadata><o xmlns='urn:x' a='1'/></metadata>\
             <devices><disk type='file'/>{devices}<video><model type='virtio'/></video></devices></domain>"
        )
    }

    #[test]
    fn a_new_vm_gets_spice_as_before_vnc_on_loopback_or_both() {
        assert_eq!(DisplayKind::default(), DisplayKind::Spice);
        assert_eq!(DisplayKind::Spice.graphics_xml(), SPICE);
        assert_eq!(DisplayKind::Vnc.graphics_xml(), VNC);
        assert_eq!(DisplayKind::Both.graphics_xml(), format!("{SPICE}{VNC}"));
        assert!(!DisplayKind::Vnc.graphics_xml().contains("passwd"));
    }

    #[test]
    fn the_words_are_the_ones_the_parameter_takes() {
        let words: Vec<_> = [DisplayKind::Spice, DisplayKind::Vnc, DisplayKind::Both]
            .map(DisplayKind::word)
            .into();
        assert_eq!(words, ["spice", "vnc", "both"]);
        for word in words {
            let kind: DisplayKind = serde_json::from_value(serde_json::json!(word)).unwrap();
            assert_eq!(kind.word(), word);
        }
        assert!(serde_json::from_value::<DisplayKind>(serde_json::json!("rdp")).is_err());
    }

    #[test]
    fn set_display_swaps_only_the_graphics_and_the_spice_audio() {
        let spice = domain(&format!("{SPICE}<audio id='1' type='spice'/>"));
        let vnc = with_display(&spice, DisplayKind::Vnc).unwrap();
        assert_eq!(
            vnc,
            domain(&format!("{VNC}<audio id='1' type='none'/>")),
            "the rest of the definition stays as it was"
        );
        let both = with_display(&vnc, DisplayKind::Both).unwrap();
        assert_eq!(
            both,
            domain(&format!("{SPICE}{VNC}<audio id='1' type='none'/>"))
        );
        let back = with_display(&both, DisplayKind::Spice).unwrap();
        assert_eq!(back, domain(&format!("{SPICE}<audio id='1' type='none'/>")));
    }

    #[test]
    fn set_display_keeps_a_spice_audio_backend_while_spice_stays() {
        let both = domain(&format!("{SPICE}{VNC}<audio id='1' type='spice'/>"));
        assert!(
            with_display(&both, DisplayKind::Spice)
                .unwrap()
                .contains("<audio id='1' type='spice'/>")
        );
    }

    #[test]
    fn set_display_adds_graphics_to_a_definition_without_any() {
        let none = domain("");
        let xml = with_display(&none, DisplayKind::Vnc).unwrap();
        assert_eq!(
            xml,
            "<domain type='kvm'><name>t</name><metadata><o xmlns='urn:x' a='1'/></metadata>\
             <devices><disk type='file'/><video><model type='virtio'/></video>"
                .to_string()
                + VNC
                + "</devices></domain>"
        );
    }

    #[test]
    fn set_display_takes_libvirts_written_back_form() {
        let written = domain(
            "<graphics type='vnc' port='-1' autoport='yes' listen='127.0.0.1'>\
             <listen type='address' address='127.0.0.1'/></graphics>\
             <graphics type='spice' port='-1' tlsPort='-1' autoport='yes'>\
             <listen type='address'/><image compression='off'/></graphics>\
             <graphics type='spice' autoport='yes' listen='::1'><listen type='address' address='::1'/></graphics>",
        );
        let xml = with_display(&written, DisplayKind::Vnc).unwrap();
        assert_eq!(xml, domain(VNC));
    }

    #[test]
    fn set_display_refuses_a_setting_it_would_drop_and_names_it() {
        for (setting, graphics) in [
            (
                "clipboard",
                "<graphics type='spice' autoport='yes'><listen type='address'/>\
                 <clipboard copypaste='no'/></graphics>",
            ),
            (
                "filetransfer",
                "<graphics type='spice' autoport='yes'><filetransfer enable='no'/></graphics>",
            ),
            (
                "channel",
                "<graphics type='spice' autoport='yes'><channel name='main' mode='secure'/></graphics>",
            ),
            (
                "gl",
                "<graphics type='spice' autoport='yes'><gl enable='yes'/></graphics>",
            ),
            (
                "keymap",
                "<graphics type='vnc' autoport='yes' keymap='fi'/>",
            ),
            (
                "websocket",
                "<graphics type='vnc' autoport='yes' websocket='-1'/>",
            ),
            (
                "sharePolicy",
                "<graphics type='vnc' autoport='yes' sharePolicy='force-shared'/>",
            ),
            (
                "compression",
                "<graphics type='spice' autoport='yes'><image compression='auto_glz'/></graphics>",
            ),
            (
                "mouse",
                "<graphics type='spice' autoport='yes'><mouse mode='client'/></graphics>",
            ),
            (
                "fromConfig",
                "<graphics type='vnc' autoport='yes'><listen type='address' fromConfig='yes'/></graphics>",
            ),
            ("autoport", "<graphics type='vnc' autoport='no'/>"),
        ] {
            let err = with_display(&domain(graphics), DisplayKind::Spice)
                .expect_err(setting)
                .to_string();
            assert!(
                err.contains(&format!("`{setting}`")),
                "{setting} is not named: {err}"
            );
        }
    }

    #[test]
    fn set_display_refuses_to_drop_spice_from_a_vm_with_spicevmc_devices() {
        // libvirt refuses `<channel type='spicevmc'>` and `<redirdev type='spicevmc'>` without SPICE graphics.
        for devices in [
            "<channel type='spicevmc'><target type='virtio' name='com.redhat.spice.0'/></channel>",
            "<redirdev bus='usb' type='spicevmc'/>",
        ] {
            let xml = domain(&format!("{SPICE}{devices}"));
            let err = with_display(&xml, DisplayKind::Vnc)
                .unwrap_err()
                .to_string();
            assert!(err.contains("spicevmc"), "{err}");
            for kind in [DisplayKind::Spice, DisplayKind::Both] {
                assert!(with_display(&xml, kind).is_ok());
            }
        }
        // Other devices of those kinds stay out of the way.
        let xml = domain(&format!(
            "{SPICE}<channel type='unix'/><redirdev bus='usb' type='tcp'/>"
        ));
        assert!(with_display(&xml, DisplayKind::Vnc).is_ok());
    }

    #[test]
    fn set_display_refuses_a_display_the_server_did_not_write() {
        for (what, graphics) in [
            ("sdl", "<graphics type='sdl'/>"),
            ("egl", "<graphics type='egl-headless'/>"),
            (
                "password",
                "<graphics type='vnc' port='-1' autoport='yes' passwd='x'/>",
            ),
            ("fixed port", "<graphics type='vnc' port='5905'/>"),
            (
                "all addresses",
                "<graphics type='vnc' autoport='yes' listen='0.0.0.0'>\
                 <listen type='address' address='0.0.0.0'/></graphics>",
            ),
            (
                "socket",
                "<graphics type='spice' autoport='no'><listen type='socket' socket='/run/s'/></graphics>",
            ),
            (
                "network",
                "<graphics type='vnc' autoport='yes'><listen type='network' network='lan'/></graphics>",
            ),
        ] {
            let err = with_display(&domain(graphics), DisplayKind::Vnc)
                .expect_err(what)
                .to_string();
            assert!(err.contains("set_display"), "{what}: {err}");
        }
        // One refused display refuses the whole change, even beside one that would pass.
        assert!(
            with_display(
                &domain(&format!("{SPICE}<graphics type='sdl'/>")),
                DisplayKind::Vnc
            )
            .is_err()
        );
    }

    #[test]
    fn set_display_refuses_a_password_in_the_form_libvirt_returns_with_the_secure_flag() {
        // `virsh dumpxml --inactive --security-info` of a VNC display with a password, and the same with a validity.
        for graphics in [
            "<graphics type='vnc' port='-1' autoport='yes' listen='127.0.0.1' passwd='testpw'>\
             <listen type='address' address='127.0.0.1'/></graphics>",
            "<graphics type='vnc' port='-1' autoport='yes' passwd='testpw' passwdValidTo='2030-01-01T00:00:00'>\
             <listen type='address'/></graphics>",
        ] {
            let err = with_display(&domain(graphics), DisplayKind::Spice)
                .unwrap_err()
                .to_string();
            assert!(err.contains("with a password"), "{err}");
            assert!(
                !err.contains("testpw"),
                "the error must not repeat the password: {err}"
            );
        }
    }

    #[test]
    fn set_display_refuses_a_definition_that_is_not_a_domain() {
        assert!(with_display("<domain>", DisplayKind::Vnc).is_err());
        assert!(with_display("<domain><name>t</name></domain>", DisplayKind::Vnc).is_err());
    }

    #[test]
    fn set_display_refuses_a_devices_element_without_a_closing_tag_instead_of_panicking() {
        // The last ten bytes of this element begin inside the two bytes of the attribute name.
        let xml = "<domain><devices é='abcd'/></domain>";
        let err = with_display(xml, DisplayKind::Vnc).unwrap_err().to_string();
        assert!(err.contains("closing tag"), "{err}");
    }

    #[test]
    fn displays_lists_the_graphics_types_in_order() {
        assert!(displays(&domain("")).unwrap().is_empty());
        assert_eq!(displays(&domain(SPICE)).unwrap(), ["spice"]);
        assert_eq!(
            displays(&domain(&format!("{VNC}{SPICE}"))).unwrap(),
            ["vnc", "spice"]
        );
        assert!(displays("<domain").is_err());
    }

    #[test]
    fn the_vnc_endpoint_is_the_live_vnc_port() {
        let live = |g: &str| domain(g);
        assert_eq!(
            vnc_endpoint(&live(
                "<graphics type='spice' port='5903' autoport='yes' listen='127.0.0.1'/>\
                 <graphics type='vnc' port='5904' autoport='yes' listen='127.0.0.1'>\
                 <listen type='address' address='127.0.0.1'/></graphics>"
            )),
            Some(("127.0.0.1".into(), 5904))
        );
        // The address of the `<listen>` child wins over the attribute, and a missing one is the loopback.
        assert_eq!(
            vnc_endpoint(&live(
                "<graphics type='vnc' port='5900' listen='10.0.0.1'><listen type='address' address='127.0.0.1'/></graphics>"
            )),
            Some(("127.0.0.1".into(), 5900))
        );
        assert_eq!(
            vnc_endpoint(&live("<graphics type='vnc' port='5901'/>")),
            Some(("127.0.0.1".into(), 5901))
        );
        // No VNC, no port yet, or nonsense.
        assert_eq!(vnc_endpoint(&live(SPICE)), None);
        assert_eq!(
            vnc_endpoint(&live("<graphics type='vnc' port='-1'/>")),
            None
        );
        assert_eq!(vnc_endpoint(&live("<graphics type='vnc' port='x'/>")), None);
        assert_eq!(vnc_endpoint("<domain"), None);
    }
}
