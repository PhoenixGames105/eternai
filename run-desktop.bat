@echo off
set "PATH=%USERPROFILE%\.cargo\bin;%PATH%"
cd /d E:\eternAI\apps\desktop
call npm run tauri dev > E:\eternAI\tauri-dev.log 2>&1
