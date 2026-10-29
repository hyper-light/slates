@echo off
rem The daemon and its clients inside one process-isolated container (the Windows container DAX cell's leg A): a
rem file written, mapped read-only by the direct map (A-175), then mapped read and write, stored, synced and read back
rem (A-186).
setlocal enabledelayedexpansion
set S=C:\slates
echo DIAG: user %USERNAME%
start "" /b %S%\slates.exe anchor --quick --shards 1
set /a tries=0
:wait
%S%\slates.exe volume list >nul 2>&1
if !errorlevel!==0 goto ready
set /a tries+=1
if !tries! gtr 120 (
  echo DIAG: the daemon never answered
  exit /b 3
)
ping -n 2 127.0.0.1 >nul
goto wait
:ready
%S%\slates.exe bootstrap root
if !errorlevel! neq 0 (
  echo DIAG: bootstrap refused
  exit /b 3
)
set V=
for /f "tokens=1,2" %%a in ('%S%\slates.exe volume create direct --bounded 16MiB') do if "%%a"=="id:" set V=%%b
echo DIAG: volume !V!
%S%\pod_direct_probe.exe write !V! /direct.bin 4194304 68
if !errorlevel! neq 0 exit /b 3
%S%\pod_direct_probe.exe map !V! /direct.bin 4194304 68
set MAPPED=!errorlevel!
echo DIAG: map exit !MAPPED!
%S%\pod_direct_probe.exe map-write !V! /direct.bin 4194304 68
set WROTE=!errorlevel!
echo DIAG: map-write exit !WROTE!
%S%\slates.exe status | findstr /c:"dax.local_map"
if !MAPPED! neq 0 exit /b !MAPPED!
exit /b !WROTE!
