Option Explicit

' Starts one of the port's commands on the install this file sits in (<root>\pool-rs\), as
' Agent Pools' own wrappers start node: Task Scheduler starts wscript.exe, a GUI-subsystem
' process, so no console window exists to be closed by hand, and Run(..., 0, True) waits, so
' the task stays Running while the command lives (IgnoreNew makes the 5-minute trigger a
' keepalive). The version is pool-rs.json's "current"; the live install gets --live, a
' rehearsal (state\rehearsal.json) does not.
Dim fs, shell, here, root, settings, current, exe, command, live

Set fs = CreateObject("Scripting.FileSystemObject")
Set shell = CreateObject("WScript.Shell")
If WScript.Arguments.Count <> 1 Then WScript.Quit 2
command = WScript.Arguments(0)
Select Case command
  Case "guardian", "serve-inference", "serve-gateway"
  Case Else
    WScript.Quit 2
End Select
here = fs.GetParentFolderName(WScript.ScriptFullName)
root = fs.GetParentFolderName(here)
settings = fs.OpenTextFile(fs.BuildPath(here, "pool-rs.json"), 1).ReadAll()
current = JsonString(settings, "current")
If current = "" Or InStr(current, "\") > 0 Or InStr(current, "/") > 0 Or InStr(current, "..") > 0 Then WScript.Quit 3
exe = fs.BuildPath(fs.BuildPath(here, current), "pools-shadow.exe")
If Not fs.FileExists(exe) Then WScript.Quit 4
live = ""
If Not fs.FileExists(fs.BuildPath(root, "state\rehearsal.json")) Then live = " --live"

shell.CurrentDirectory = root
WScript.Quit shell.Run(Quote(exe) & " " & command & " " & Quote(root) & live, 0, True)

' Minimal reader for one string field (as Run-Pool-Guardian.vbs reads install.json).
Function JsonString(text, key)
  Dim re, matches
  Set re = New RegExp
  re.Pattern = """" & key & """\s*:\s*""((?:[^""\\]|\\.)*)"""
  Set matches = re.Execute(text)
  If matches.Count = 0 Then
    JsonString = ""
  Else
    JsonString = Replace(Replace(matches(0).SubMatches(0), "\\", "\"), "\/", "/")
  End If
End Function

Function Quote(value)
  Quote = Chr(34) & value & Chr(34)
End Function
