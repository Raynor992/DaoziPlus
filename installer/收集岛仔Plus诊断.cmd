@echo off
rem Portable package: the diagnostics script is embedded in daozai-plus.exe.
start "" "%~dp0daozai-plus.exe" --collect-diagnostics
