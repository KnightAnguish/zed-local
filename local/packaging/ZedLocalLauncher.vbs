Option Explicit
Dim fso, shell, root, updater
Set fso = CreateObject("Scripting.FileSystemObject")
Set shell = CreateObject("WScript.Shell")
root = fso.GetParentFolderName(WScript.ScriptFullName)
updater = fso.BuildPath(root, "Update-ZedLocal.ps1")
If fso.FileExists(updater) Then
    shell.Run "powershell.exe -NoProfile -ExecutionPolicy Bypass -File """ & updater & """ -Launch", 0, False
Else
    Dim exe
    exe = fso.BuildPath(root, "zedlocal.exe")
    If fso.FileExists(exe) Then
        shell.Run """" & exe & """", 1, False
    End If
End If
