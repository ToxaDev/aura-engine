@echo off
chcp 65001 >nul
echo ==================================================
echo AuraEngine -- Hybrid-Phase Comparison Tool
echo ==================================================
cd /d "%~dp0"
call venv\Scripts\python.exe compare.py
pause
