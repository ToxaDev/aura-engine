@echo off
chcp 65001 >nul
title AuraEngine -- Hybrid-Phase Verification
echo.
echo ===========================================================
echo   AuraEngine -- Hybrid-Phase Verification Tool (100%% Proof)
echo ===========================================================
echo.
echo   This tool proves that the Hybrid-Phase feature works
echo   by analyzing pre-ringing elimination at transient attacks.
echo.
echo   You need 3 files:
echo     1. Source audio       (44.1/48kHz original FLAC/WAV)
echo     2. Linear-only output (converted with Hybrid-Phase OFF)
echo     3. Hybrid output      (converted with Hybrid-Phase ON)
echo.
echo -----------------------------------------------------------

cd /d "%~dp0"

REM -- Clean old results --
if exist "verify_results" (
    echo   [*] Cleaning previous results...
    del /q "verify_results\*.png" 2>nul
    echo   [OK] Old graphs removed.
) else (
    echo   [*] Creating results directory...
    mkdir "verify_results"
    echo   [OK] Created verify_results\
)
echo.

REM -- Check venv --
if not exist "venv\Scripts\python.exe" (
    echo   [!] Virtual environment not found.
    echo   [!] Please run start.bat first to set up the environment.
    echo.
    pause
    exit /b 1
)

REM -- Run verification --
if "%~1"=="" (
    REM GUI mode (no arguments)
    echo   Launching GUI...
    echo.
    venv\Scripts\python.exe verify_hybrid_phase.py
) else (
    REM CLI mode (3 file arguments)
    echo   Running in CLI mode...
    echo   Source:  %~nx1
    echo   Linear: %~nx2
    echo   Hybrid: %~nx3
    echo.
    venv\Scripts\python.exe verify_hybrid_phase.py "%~1" "%~2" "%~3"
)

echo.
echo -----------------------------------------------------------

if exist "verify_results\summary.png" (
    echo   [OK] Results saved to: verify_results\
    echo.
    echo   Opening results folder...
    start "" "verify_results"
) else (
    echo   [!] No results generated.
)

echo.
pause
