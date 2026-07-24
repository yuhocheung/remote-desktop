@echo off
call "C:\Program Files (x86)\Microsoft Visual Studio\2019\BuildTools\VC\Auxiliary\Build\vcvars64.bat"
set "PATH=C:\Users\69497\.cargo\bin;%PATH%"
cd /d C:\Users\69497\WorkBuddy\remote-desktop\core
cargo test -p rdcore-ffi --lib
