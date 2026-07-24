@echo off
call "C:\Program Files (x86)\Microsoft Visual Studio\2019\BuildTools\VC\Auxiliary\Build\vcvars64.bat"
set "PATH=C:\Users\69497\.cargo\bin;%PATH%"
cd /d C:\Users\69497\WorkBuddy\remote-desktop\core
cargo build -p rdcore-ffi --release || exit /b 1
copy /y "C:\Users\69497\WorkBuddy\remote-desktop\target\release\rdcore_ffi.dll" "C:\Users\69497\WorkBuddy\remote-desktop\flutter\windows\rdcore\rdcore_ffi.dll"
copy /y "C:\Users\69497\WorkBuddy\remote-desktop\target\release\rdcore_ffi.dll.lib" "C:\Users\69497\WorkBuddy\remote-desktop\flutter\windows\rdcore\rdcore_ffi.dll.lib"
