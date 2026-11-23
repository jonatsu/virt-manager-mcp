// The final steps of a Windows directory push, which push-place.ps1 compiles with Add-Type and runs as SYSTEM. A push
// stages its tree in %SystemRoot%\Temp, where other users cannot rename or delete the push's entries. Everything it
// then does in the target's parent happens here, in one process, on open handles: a standard user who owns the parent
// may rename entries in it, so a step that found an entry by name again could be redirected through a junction.
//
// The helper opens the target's path one component at a time, each relative to the one before and without following a
// reparse point, and keeps every handle open without delete sharing, which pins the parent and every directory above
// it. It creates missing parents the same way. It moves the staged tree into the parent, or copies it when the parent
// is on another volume, moves an existing target aside, gives the new tree its parent's access list, and deletes the
// old tree. A failure before the old tree's deletion deletes the new tree and puts the old one back.
//
// Windows PowerShell 5.1 compiles this with its C# 5 compiler, so the code uses no later language feature.

using System;
using System.Collections.Generic;
using System.ComponentModel;
using System.IO;
using System.Runtime.InteropServices;
using System.Text.RegularExpressions;
using Microsoft.Win32.SafeHandles;

public static class VmmPush
{
    // Exit statuses; push.rs maps each to its message.
    public const int Done = 0, Exists = 11, NotADirectory = 12, NoParents = 13, OldStuck = 14, NewStuckOldBack = 15,
      NewStuck = 16, BothStuck = 18, Busy = 20, Redirected = 21, NotInherited = 22, ReservedName = 24,
      NotCopied = 25, OldLeft = 30;

    [StructLayout(LayoutKind.Sequential)]
    struct UNICODE_STRING { public ushort Length; public ushort MaximumLength; public IntPtr Buffer; }

    [StructLayout(LayoutKind.Sequential)]
    struct OBJECT_ATTRIBUTES
    {
        public int Length; public IntPtr RootDirectory; public IntPtr ObjectName; public uint Attributes;
        public IntPtr SecurityDescriptor; public IntPtr SecurityQualityOfService;
    }

    [StructLayout(LayoutKind.Sequential)]
    struct IO_STATUS_BLOCK { public IntPtr Status; public IntPtr Information; }

    [DllImport("ntdll.dll")]
    static extern int NtCreateFile(out SafeFileHandle handle, uint access, ref OBJECT_ATTRIBUTES attributes,
      out IO_STATUS_BLOCK status, IntPtr allocation, uint fileAttributes, uint share, uint disposition, uint options,
      IntPtr ea, uint eaLength);

    [DllImport("ntdll.dll")]
    static extern int NtSetInformationFile(SafeFileHandle handle, out IO_STATUS_BLOCK status, IntPtr info, uint length,
      int infoClass);

    [DllImport("ntdll.dll")]
    static extern int RtlNtStatusToDosError(int status);

    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool GetFileInformationByHandleEx(SafeFileHandle handle, int infoClass, IntPtr buffer, uint size);

    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool SetFileInformationByHandle(SafeFileHandle handle, int infoClass, IntPtr buffer, uint size);

    [DllImport("kernel32.dll")]
    static extern IntPtr LocalFree(IntPtr memory);

    [DllImport("advapi32.dll")]
    static extern uint SetSecurityInfo(SafeFileHandle handle, int objectType, uint info, IntPtr owner, IntPtr group,
      IntPtr dacl, IntPtr sacl);

    [DllImport("advapi32.dll", SetLastError = true)]
    static extern bool InitializeAcl(IntPtr acl, uint length, uint revision);

    [DllImport("advapi32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
    static extern bool ConvertStringSecurityDescriptorToSecurityDescriptorW(string text, uint revision,
      out IntPtr descriptor, IntPtr size);

    // Access rights, share modes, dispositions and options of NtCreateFile.
    const uint ListDir = 0x1, AddFile = 0x2, AddSubdir = 0x4, ReadData = 0x1, WriteData = 0x2, ReadAttributes = 0x80,
      WriteAttributes = 0x100, Delete = 0x10000, ReadControl = 0x20000, WriteDac = 0x40000, Synchronize = 0x100000;
    const uint ShareRead = 0x1, ShareReadWrite = 0x3, ShareAll = 0x7;
    const uint DispOpen = 1, DispCreate = 2, DispOpenIf = 3;
    const uint DirectoryFile = 0x1, NonDirectoryFile = 0x40, SynchronousIo = 0x20, OpenForBackup = 0x4000,
      OpenReparsePoint = 0x200000;
    const uint CaseInsensitive = 0x40, DontReparse = 0x1000;

    const uint AttrReadOnly = 0x1, AttrHidden = 0x2, AttrSystem = 0x4, AttrDirectory = 0x10, AttrArchive = 0x20,
      AttrReparsePoint = 0x400, AttrNotIndexed = 0x2000;
    // Junctions, symbolic links and mount points carry this bit in their reparse tag; cloud-file placeholders do not.
    const uint NameSurrogate = 0x20000000;

    const int StatusNotFound = unchecked((int)0xC0000034), StatusPathNotFound = unchecked((int)0xC000003A),
      StatusCollision = unchecked((int)0xC0000035), StatusAccessDenied = unchecked((int)0xC0000022),
      StatusSharingViolation = unchecked((int)0xC0000043), StatusNotADirectory = unchecked((int)0xC0000103);

    // Only SYSTEM and Administrators, inherited by everything inside, as push-mkdir.ps1 sets.
    const string Protected = "D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)";

    // A failure that ends Place with an exit status.
    sealed class Failure : Exception
    {
        public readonly int Code;
        public Failure(int code, string message) : base(message) { Code = code; }
    }

    // An NTSTATUS from one call, kept so Place can tell a busy entry or a name collision from other failures.
    sealed class NtFailure : Win32Exception
    {
        public readonly int Status;
        public NtFailure(string what, int status)
          : base(RtlNtStatusToDosError(status), string.Format("{0}: NTSTATUS 0x{1:X8}", what, status))
        {
            Status = status;
        }
        public bool IsBusy { get { return Status == StatusSharingViolation || Status == StatusAccessDenied; } }
    }

    static readonly Regex Reserved = new Regex(
      @"^(CON|PRN|AUX|NUL|COM[1-9\u00B9\u00B2\u00B3]|LPT[1-9\u00B9\u00B2\u00B3])(\..*)?$",
      RegexOptions.IgnoreCase | RegexOptions.CultureInvariant);

    // Opens `name` relative to `root`, or as a full NT path when `root` is null, never following a reparse point at the
    // last component. `kind` is 1 for a directory, -1 for a file, 0 for either.
    static SafeFileHandle Open(SafeFileHandle root, string name, uint access, uint share, int kind, uint disposition,
        bool dontReparse, IntPtr descriptor)
    {
        var text = Marshal.StringToHGlobalUni(name);
        var unicode = Marshal.AllocHGlobal(Marshal.SizeOf(typeof(UNICODE_STRING)));
        bool added = false;
        try
        {
            var us = new UNICODE_STRING();
            us.Length = checked((ushort)(name.Length * 2));
            us.MaximumLength = us.Length;
            us.Buffer = text;
            Marshal.StructureToPtr(us, unicode, false);
            var oa = new OBJECT_ATTRIBUTES();
            oa.Length = Marshal.SizeOf(typeof(OBJECT_ATTRIBUTES));
            if (root != null)
            {
                root.DangerousAddRef(ref added);
                oa.RootDirectory = root.DangerousGetHandle();
            }
            oa.ObjectName = unicode;
            oa.Attributes = CaseInsensitive | (dontReparse ? DontReparse : 0);
            oa.SecurityDescriptor = descriptor;
            uint options = SynchronousIo | OpenForBackup | OpenReparsePoint |
              (kind > 0 ? DirectoryFile : kind < 0 ? NonDirectoryFile : 0);
            SafeFileHandle handle;
            IO_STATUS_BLOCK status;
            int result = NtCreateFile(out handle, access | Synchronize | ReadAttributes, ref oa, out status,
              IntPtr.Zero, 0x80, share, disposition, options, IntPtr.Zero, 0);
            if (result != 0) throw new NtFailure("opening " + name, result);
            return handle;
        }
        finally
        {
            if (added) root.DangerousRelease();
            Marshal.FreeHGlobal(unicode);
            Marshal.FreeHGlobal(text);
        }
    }

    static uint[] AttributesAndTag(SafeFileHandle handle)
    {
        var buffer = Marshal.AllocHGlobal(8);
        try
        {
            if (!GetFileInformationByHandleEx(handle, 9, buffer, 8)) throw new Win32Exception();
            return new uint[] { (uint)Marshal.ReadInt32(buffer, 0), (uint)Marshal.ReadInt32(buffer, 4) };
        }
        finally { Marshal.FreeHGlobal(buffer); }
    }

    static bool IsReparsePoint(SafeFileHandle handle)
    {
        return (AttributesAndTag(handle)[0] & AttrReparsePoint) != 0;
    }

    static uint Links(SafeFileHandle handle)
    {
        var buffer = Marshal.AllocHGlobal(24);
        try
        {
            if (!GetFileInformationByHandleEx(handle, 1, buffer, 24)) throw new Win32Exception();
            return (uint)Marshal.ReadInt32(buffer, 16);
        }
        finally { Marshal.FreeHGlobal(buffer); }
    }

    static long VolumeSerial(SafeFileHandle handle)
    {
        var buffer = Marshal.AllocHGlobal(24);
        try
        {
            if (!GetFileInformationByHandleEx(handle, 18, buffer, 24)) throw new Win32Exception();
            return Marshal.ReadInt64(buffer, 0);
        }
        finally { Marshal.FreeHGlobal(buffer); }
    }

    // Renames the entry open as `handle` to `name` in the directory open as `root`, never replacing an entry there.
    static void Rename(SafeFileHandle handle, SafeFileHandle root, string name)
    {
        int length = 20 + name.Length * 2;
        var info = Marshal.AllocHGlobal(length);
        bool added = false;
        try
        {
            for (int i = 0; i < length; i++) Marshal.WriteByte(info, i, 0);
            root.DangerousAddRef(ref added);
            Marshal.WriteIntPtr(info, 8, root.DangerousGetHandle());
            Marshal.WriteInt32(info, 16, name.Length * 2);
            Marshal.Copy(name.ToCharArray(), 0, info + 20, name.Length);
            IO_STATUS_BLOCK status;
            int result = NtSetInformationFile(handle, out status, info, (uint)length, 10);
            if (result != 0) throw new NtFailure("renaming to " + name, result);
        }
        finally
        {
            if (added) root.DangerousRelease();
            Marshal.FreeHGlobal(info);
        }
    }

    // Deletes the entry open as `handle` at once, read-only or not; a reparse point goes as a link.
    static void Remove(SafeFileHandle handle)
    {
        var info = Marshal.AllocHGlobal(4);
        try
        {
            // FILE_DISPOSITION_DELETE | POSIX_SEMANTICS | IGNORE_READONLY_ATTRIBUTE
            Marshal.WriteInt32(info, 0, 0x1 | 0x2 | 0x10);
            IO_STATUS_BLOCK status;
            int result = NtSetInformationFile(handle, out status, info, 4, 64);
            if (result != 0) throw new NtFailure("deleting", result);
        }
        finally { Marshal.FreeHGlobal(info); }
    }

    sealed class Entry
    {
        public string Name;
        public uint Attributes;
        public bool IsDirectory { get { return (Attributes & AttrDirectory) != 0; } }
        public bool IsReparsePoint { get { return (Attributes & AttrReparsePoint) != 0; } }
    }

    // The entries of the directory open as `directory`, read through its handle.
    static List<Entry> List(SafeFileHandle directory)
    {
        var entries = new List<Entry>();
        const int size = 64 * 1024;
        var buffer = Marshal.AllocHGlobal(size);
        try
        {
            // FileFullDirectoryRestartInfo, then FileFullDirectoryInfo, until ERROR_NO_MORE_FILES.
            int infoClass = 15;
            while (GetFileInformationByHandleEx(directory, infoClass, buffer, size))
            {
                infoClass = 14;
                int offset = 0;
                while (true)
                {
                    var record = buffer + offset;
                    var name = Marshal.PtrToStringUni(record + 68, Marshal.ReadInt32(record, 60) / 2);
                    if (name != "." && name != "..")
                    {
                        var entry = new Entry();
                        entry.Name = name;
                        entry.Attributes = (uint)Marshal.ReadInt32(record, 56);
                        entries.Add(entry);
                    }
                    int next = Marshal.ReadInt32(record, 0);
                    if (next == 0) break;
                    offset += next;
                }
            }
            int error = Marshal.GetLastWin32Error();
            if (error != 18) throw new Win32Exception(error, "listing a directory");
        }
        finally { Marshal.FreeHGlobal(buffer); }
        return entries;
    }

    // Deletes everything inside the directory open as `directory`, never following a reparse point.
    static void Empty(SafeFileHandle directory)
    {
        foreach (var entry in List(directory))
        {
            using (var handle = Open(directory, entry.Name, Delete | ListDir, ShareAll, 0, DispOpen, false,
              IntPtr.Zero))
            {
                if (entry.IsDirectory && !IsReparsePoint(handle)) Empty(handle);
                Remove(handle);
            }
        }
    }

    static void DeleteTree(SafeFileHandle directory)
    {
        Empty(directory);
        Remove(directory);
    }

    // Turns inheritance on for the entry open as `handle`, with no explicit entries; SetSecurityInfo computes the
    // inherited ones from the parent.
    static void Inherit(SafeFileHandle handle)
    {
        var acl = Marshal.AllocHGlobal(64);
        try
        {
            if (!InitializeAcl(acl, 64, 2)) throw new Win32Exception();
            // DACL_SECURITY_INFORMATION | UNPROTECTED_DACL_SECURITY_INFORMATION
            uint error = SetSecurityInfo(handle, 1, 0x4 | 0x20000000, IntPtr.Zero, IntPtr.Zero, acl, IntPtr.Zero);
            if (error != 0) throw new Win32Exception((int)error, "setting an access list");
        }
        finally { Marshal.FreeHGlobal(acl); }
    }

    // Gives the directory open as `directory` and everything in it the access list its parent passes on. Each entry is
    // opened while its directory is still protected, so it can only be one the push made, and held without delete
    // sharing until it is done. Reparse points, and files with more than one link, are left alone.
    static void InheritTree(SafeFileHandle directory)
    {
        var children = new List<SafeFileHandle>();
        var directories = new List<bool>();
        try
        {
            foreach (var entry in List(directory))
            {
                if (entry.IsReparsePoint) continue;
                uint access = WriteDac | ReadControl | (entry.IsDirectory ? ListDir : 0);
                int kind = entry.IsDirectory ? 1 : -1;
                var handle = Open(directory, entry.Name, access, ShareReadWrite, kind, DispOpen, false, IntPtr.Zero);
                if (IsReparsePoint(handle) || (!entry.IsDirectory && Links(handle) > 1))
                {
                    handle.Dispose();
                    continue;
                }
                children.Add(handle);
                directories.Add(entry.IsDirectory);
            }
            Inherit(directory);
            for (int i = 0; i < children.Count; i++)
            {
                if (directories[i]) InheritTree(children[i]); else Inherit(children[i]);
            }
        }
        finally
        {
            foreach (var child in children) child.Dispose();
        }
    }

    // Copies the basic information of `source` to `target`: its times, and for a file, the attributes a copy keeps.
    static void CopyBasicInfo(SafeFileHandle source, SafeFileHandle target, bool directory)
    {
        var info = Marshal.AllocHGlobal(40);
        try
        {
            if (!GetFileInformationByHandleEx(source, 0, info, 40)) throw new Win32Exception();
            uint attributes = (uint)Marshal.ReadInt32(info, 32);
            uint kept = attributes & (AttrReadOnly | AttrHidden | AttrSystem | AttrArchive | AttrNotIndexed);
            // Zero leaves the attributes unchanged.
            Marshal.WriteInt32(info, 32, directory ? 0 : (int)kept);
            if (!SetFileInformationByHandle(target, 0, info, 40)) throw new Win32Exception();
        }
        finally { Marshal.FreeHGlobal(info); }
    }

    // Copies the tree open as `source` into the empty directory open as `target`, skipping reparse points. New entries
    // inherit the target's protected access list.
    static void CopyTree(SafeFileHandle source, SafeFileHandle target)
    {
        foreach (var entry in List(source))
        {
            if (entry.IsReparsePoint) continue;
            if (entry.IsDirectory)
            {
                using (var from = Open(source, entry.Name, ListDir, ShareRead, 1, DispOpen, false, IntPtr.Zero))
                using (var to = Open(target, entry.Name, ListDir | AddFile | AddSubdir | WriteAttributes, 0, 1,
                    DispCreate, false, IntPtr.Zero))
                {
                    CopyTree(from, to);
                    CopyBasicInfo(from, to, true);
                }
            }
            else
            {
                using (var from = Open(source, entry.Name, ReadData, ShareRead, -1, DispOpen, false, IntPtr.Zero))
                using (var to = Open(target, entry.Name, WriteData | WriteAttributes, 0, -1, DispCreate, false,
                    IntPtr.Zero))
                {
                    using (var read = new FileStream(from, FileAccess.Read, 65536, false))
                    using (var write = new FileStream(to, FileAccess.Write, 65536, false))
                    {
                        read.CopyTo(write);
                        write.Flush();
                    }
                }
                // The streams closed both handles; the times and attributes go on through fresh ones.
                using (var from = Open(source, entry.Name, ReadAttributes, ShareRead, -1, DispOpen, false, IntPtr.Zero))
                using (var to = Open(target, entry.Name, WriteAttributes, 0, -1, DispOpen, false, IntPtr.Zero))
                {
                    CopyBasicInfo(from, to, false);
                }
            }
        }
    }

    // Creates a directory named `name` in `parent` with the protected access list.
    static SafeFileHandle CreateProtected(SafeFileHandle parent, string name, uint access)
    {
        IntPtr descriptor;
        if (!ConvertStringSecurityDescriptorToSecurityDescriptorW(Protected, 1, out descriptor, IntPtr.Zero))
        {
            throw new Win32Exception();
        }
        try
        {
            return Open(parent, name, access, ShareReadWrite, 1, DispCreate, false, descriptor);
        }
        finally { LocalFree(descriptor); }
    }

    // Runs the final steps of a push: `target` is a drive path such as C:\work\app, `staging` the unpacked tree in
    // %SystemRoot%\Temp, `copyName` and `oldName` the names in the parent for a copy and for the old tree. Writes the
    // reason for a failure to `error` and returns an exit status.
    public static int Place(string target, string staging, string copyName, string oldName, bool replace,
        TextWriter error)
    {
        var held = new List<SafeFileHandle>();
        try
        {
            return Run(target, staging, copyName, oldName, replace, held);
        }
        catch (Failure f)
        {
            error.Write(f.Message);
            return f.Code;
        }
        catch (Exception e)
        {
            // Every step that moves a tree reports its own failure, so this one comes before anything moved, or from a
            // check between the moves that the messages above do not cover.
            error.Write("the push's final step failed unexpectedly: " + e.Message);
            return 1;
        }
        finally
        {
            for (int i = held.Count - 1; i >= 0; i--) held[i].Dispose();
        }
    }

    static int Run(string target, string staging, string copyName, string oldName, bool replace,
        List<SafeFileHandle> held)
    {
        if (target.Length < 4 || target[1] != ':' || target[2] != '\\')
        {
            throw new Failure(NoParents, target + ": not a drive path below a drive's root");
        }
        var parts = new List<string>(target.Substring(3).Split(new[] { '\\' }, StringSplitOptions.RemoveEmptyEntries));
        foreach (var part in parts)
        {
            if (Reserved.IsMatch(part))
            {
                throw new Failure(ReservedName, target + ": '" + part + "' is a device name that Windows reserves");
            }
        }
        if (parts.Count == 0) throw new Failure(NoParents, target + ": not a drive path below a drive's root");
        string name = parts[parts.Count - 1];
        parts.RemoveAt(parts.Count - 1);

        // The parent, one component at a time.
        uint dirAccess = ListDir | AddFile | AddSubdir | ReadControl;
        string reached = target.Substring(0, 3);
        SafeFileHandle parent;
        try
        {
            parent = Open(null, @"\??\" + reached, dirAccess, ShareReadWrite, 1, DispOpen, false, IntPtr.Zero);
        }
        catch (Win32Exception e)
        {
            throw new Failure(NoParents, reached + ": " + e.Message);
        }
        held.Add(parent);
        foreach (var part in parts)
        {
            reached = Path.Combine(reached, part);
            SafeFileHandle next;
            uint[] info;
            try
            {
                next = Open(parent, part, dirAccess, ShareReadWrite, 1, DispOpenIf, false, IntPtr.Zero);
                held.Add(next);
                info = AttributesAndTag(next);
            }
            catch (Win32Exception e)
            {
                var nt = e as NtFailure;
                if (nt != null && nt.Status == StatusNotADirectory)
                {
                    throw new Failure(NoParents, reached + " exists and is not a directory");
                }
                throw new Failure(NoParents, "cannot open or create " + reached + ": " + e.Message);
            }
            if ((info[0] & AttrReparsePoint) != 0 && (info[1] & NameSurrogate) != 0)
            {
                throw new Failure(Redirected, reached + string.Format(" is a junction, a symbolic link or a mount " +
                  "point (reparse tag 0x{0:X8}), so the push stopped there", info[1]));
            }
            parent = next;
        }

        // The staged tree, in the parent under its own name, or copied into a protected directory there. The staging
        // root stays open, so a failure before the inheritance can move the tree back for a retry.
        SafeFileHandle staged, home;
        string stagingName = Path.GetFileName(staging);
        try
        {
            home = Open(null, @"\??\" + Path.GetDirectoryName(staging), ListDir | AddSubdir, ShareReadWrite, 1,
              DispOpen, true, IntPtr.Zero);
            held.Add(home);
            staged = Open(null, @"\??\" + staging, Delete | WriteDac | ReadControl | ListDir, ShareReadWrite, 1,
              DispOpen, true, IntPtr.Zero);
            held.Add(staged);
        }
        catch (Win32Exception e)
        {
            throw new Failure(NewStuck, "cannot open the unpacked tree " + staging + ": " + e.Message);
        }
        bool sameVolume;
        try
        {
            sameVolume = VolumeSerial(staged) == VolumeSerial(parent);
        }
        catch (Win32Exception e)
        {
            throw new Failure(NewStuck, "cannot tell which volume " + reached + " is on: " + e.Message);
        }
        SafeFileHandle fresh;
        string freshName;
        if (sameVolume)
        {
            freshName = Path.GetFileName(staging);
            try
            {
                Rename(staged, parent, freshName);
            }
            catch (NtFailure e)
            {
                throw new Failure(e.IsBusy ? Busy : NewStuck,
                  "cannot move the unpacked tree into " + reached + ": " + e.Message);
            }
            fresh = staged;
        }
        else
        {
            freshName = copyName;
            try
            {
                uint access = Delete | WriteDac | ReadControl | ListDir | AddFile | AddSubdir;
                fresh = CreateProtected(parent, copyName, access);
                held.Add(fresh);
            }
            catch (Win32Exception e)
            {
                throw new Failure(NotCopied, "cannot create " + Path.Combine(reached, copyName) + ": " + e.Message);
            }
            try
            {
                CopyTree(staged, fresh);
            }
            catch (Exception e)
            {
                string left = Discard(fresh) ? "" : "; the partial copy is left at " + Path.Combine(reached, copyName);
                throw new Failure(NotCopied, "copying the tree to " + reached + " failed: " + e.Message + left);
            }
        }
        string freshPath = Path.Combine(reached, freshName);
        // Undoes the new tree after a failure before its inheritance: a moved tree goes back to the staging root, still
        // protected, so a retry finds it there; a copy is deleted. Returns nothing, or the end of the failure's message
        // saying where the tree stays when that fails.
        Func<string> abandon = () =>
        {
            if (sameVolume && PutBack(fresh, home, stagingName)) return "";
            return Discard(fresh) ? "" : "; undoing the new tree failed too, so it stays at " + freshPath;
        };

        // An existing target moves aside.
        SafeFileHandle old = null;
        try
        {
            old = Open(parent, name, Delete | ListDir, ShareReadWrite, 0, DispOpen, false, IntPtr.Zero);
            held.Add(old);
        }
        catch (NtFailure e)
        {
            if (e.Status != StatusNotFound && e.Status != StatusPathNotFound)
            {
                throw new Failure(e.IsBusy ? Busy : OldStuck, "cannot open " + target + ": " + e.Message +
                  abandon());
            }
        }
        string oldPath = Path.Combine(reached, oldName);
        if (old != null)
        {
            if (!replace) throw new Failure(Exists, target + " exists" + abandon());
            uint[] info;
            try
            {
                info = AttributesAndTag(old);
            }
            catch (Win32Exception e)
            {
                throw new Failure(OldStuck, "cannot read " + target + ": " + e.Message + abandon());
            }
            if ((info[0] & AttrDirectory) == 0 || (info[0] & AttrReparsePoint) != 0)
            {
                throw new Failure(NotADirectory,
                  target + " is not a directory, or is a link" + abandon());
            }
            try
            {
                Rename(old, parent, oldName);
            }
            catch (NtFailure e)
            {
                throw new Failure(e.IsBusy ? Busy : OldStuck, "cannot move " + target + " aside: " + e.Message +
                  abandon());
            }
        }

        // The new tree takes the target's name.
        try
        {
            Rename(fresh, parent, name);
        }
        catch (NtFailure e)
        {
            int code = e.Status == StatusCollision ? Exists
              : e.IsBusy ? Busy
              : old != null ? NewStuckOldBack
              : NewStuck;
            if (old != null && !PutBack(old, parent, name))
            {
                throw new Failure(BothStuck, "cannot move the new tree into " + target + " (" + e.Message +
                  "), nor the old tree back; the old tree is at " + oldPath + " and the new one at " + freshPath);
            }
            throw new Failure(code,
              "cannot move the new tree into " + target + ": " + e.Message + abandon());
        }

        // The new tree gets its parent's access list; one failure is retried.
        try
        {
            try
            {
                InheritTree(fresh);
            }
            catch (Win32Exception)
            {
                InheritTree(fresh);
            }
        }
        catch (Win32Exception e)
        {
            string reason = "giving " + target + " its parent's access list failed twice: " + e.Message;
            if (!Discard(fresh))
            {
                throw new Failure(BothStuck, reason + "; deleting the new tree failed too, so it stays at " + target +
                  (old != null ? ", and the old tree is at " + oldPath : ""));
            }
            if (old != null && !PutBack(old, parent, name))
            {
                throw new Failure(BothStuck, reason + "; the old tree could not move back and is at " + oldPath);
            }
            throw new Failure(NotInherited, reason);
        }

        if (old != null)
        {
            try
            {
                DeleteTree(old);
            }
            catch (Win32Exception e)
            {
                throw new Failure(OldLeft,
                  "the new tree is in place, but deleting the old tree at " + oldPath + " failed: " + e.Message);
            }
        }
        return Done;
    }

    // Deletes the new tree after a failure, and tells whether that worked.
    static bool Discard(SafeFileHandle fresh)
    {
        try
        {
            DeleteTree(fresh);
            return true;
        }
        catch (Win32Exception)
        {
            return false;
        }
    }

    // Renames the entry open as `handle` to `name` in `parent`, and tells whether that worked.
    static bool PutBack(SafeFileHandle handle, SafeFileHandle parent, string name)
    {
        try
        {
            Rename(handle, parent, name);
            return true;
        }
        catch (Win32Exception)
        {
            return false;
        }
    }
}
