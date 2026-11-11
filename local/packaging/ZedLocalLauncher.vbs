Option Explicit
Dim fso, shell, root, launcher
Set fso = CreateObject("Scripting.FileSystemObject")
Set shell = CreateObject("WScript.Shell")
root = fso.GetParentFolderName(WScript.ScriptFullName)
launcher = fso.BuildPath(root, "Launch-ZedLocal.ps1")
If fso.FileExists(launcher) Then
    shell.Run "powershell.exe -NoProfile -ExecutionPolicy Bypass -File """ & launcher & """", 0, False
End If
