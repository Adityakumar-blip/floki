@echo off
setlocal
set "VCVARS="
set "VSROOT="
for %%E in (BuildTools Community Professional Enterprise) do (
  if exist "%ProgramFiles(x86)%\Microsoft Visual Studio\2022\%%E\VC\Auxiliary\Build\vcvars64.bat" (
    set "VCVARS=%ProgramFiles(x86)%\Microsoft Visual Studio\2022\%%E\VC\Auxiliary\Build\vcvars64.bat"
    set "VSROOT=%ProgramFiles(x86)%\Microsoft Visual Studio\2022\%%E"
  )
)
if not defined VCVARS (
  echo Visual Studio 2022 C++ build tools were not found.
  exit /b 1
)
call "%VCVARS%" >nul
if errorlevel 1 exit /b 1
if exist "%VSROOT%\Common7\IDE\CommonExtensions\Microsoft\CMake\CMake\bin\cmake.exe" set "PATH=%VSROOT%\Common7\IDE\CommonExtensions\Microsoft\CMake\CMake\bin;%PATH%"
if "%~1"=="" (
  cargo run --release
) else (
  cargo %*
)
exit /b %errorlevel%
