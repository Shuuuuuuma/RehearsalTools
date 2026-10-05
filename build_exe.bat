@echo off
cd /d %~dp0
echo Building GakumasRehearsalTool.exe (this takes a few minutes)...
python -m pip install -r requirements.txt
python build_exe.py %*
pause
