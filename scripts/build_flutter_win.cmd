@echo off
call "C:\Program Files (x86)\Microsoft Visual Studio\2019\BuildTools\VC\Auxiliary\Build\vcvars64.bat"
set "PATH=C:\Windows\System32\WindowsPowerShell\v1.0;%PATH%"
set "NINJA=C:\Program Files (x86)\Microsoft Visual Studio\2019\BuildTools\Common7\IDE\CommonExtensions\Microsoft\CMake\Ninja\ninja.exe"
cd /d C:\Users\69497\WorkBuddy\remote-desktop\flutter\build\windows\x64-ninja
"%NINJA%" || exit /b 1
"C:\Program Files (x86)\Microsoft Visual Studio\2019\BuildTools\Common7\IDE\CommonExtensions\Microsoft\CMake\CMake\bin\cmake.exe" --install . --config Release --component Runtime || exit /b 1
