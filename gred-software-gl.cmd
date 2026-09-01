@echo off
rem Launch gred forcing Mesa's llvmpipe software OpenGL.
rem Use this on a machine with no usable GPU driver (headless server, plain RDP),
rem where the normal renderer crashes. The Mesa DLLs (opengl32.dll +
rem libgallium_wgl.dll) must sit next to gred.exe -- run.ps1 copies them from
rem .\mesa\, or drop them in target\release\ yourself.
setlocal
cd /d "%~dp0"
if exist "target\release\gred.exe" (
    target\release\gred.exe --software %*
) else (
    target\debug\gred.exe --software %*
)
