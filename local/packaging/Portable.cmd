@echo off
setlocal
set "ROOT=%~dp0"
if not exist "%ROOT%UserData" mkdir "%ROOT%UserData"
start "" "%ROOT%zedlocal.exe" --user-data-dir "%ROOT%UserData"
endlocal
