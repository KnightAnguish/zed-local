@echo off
setlocal
powershell.exe -NoProfile -ExecutionPolicy Bypass -File "%~dp0Update-ZedLocal.ps1" -CheckOnly
set "RESULT=%ERRORLEVEL%"
echo.
if not "%RESULT%"=="0" echo Update check failed with exit code %RESULT%.
pause
exit /b %RESULT%
