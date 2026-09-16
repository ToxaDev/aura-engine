@echo off
chcp 65001 >nul
setlocal EnableDelayedExpansion
title AuraEngine FIR Optimizer

cd /d "%~dp0"

REM ── Bootstrap virtual environment on first run ─────────────────────────
if not exist "venv\Scripts\python.exe" (
    echo.
    echo [!] Virtual environment not found. Creating...
    python -m venv venv
    echo [+] Installing PyTorch with CUDA 12.4...
    venv\Scripts\pip install torch --index-url https://download.pytorch.org/whl/cu124
    echo [+] Installing dependencies...
    venv\Scripts\pip install -r requirements.txt
    echo.
    echo [OK] Setup complete!
    echo.
)

set "PY=venv\Scripts\python.exe"

:menu
cls
echo ==============================================
echo   AuraEngine FIR Filter Optimizer
echo ==============================================
echo.
echo   [1] Interactive mode
echo       Pick one tap count + current source/target rate.
echo.
echo   [2] Generate ALL filters for ALL ratios  ^<-- recommended once
echo       4 sizes (1M, 5M, 10M, 30M)
echo       x 2 sources (44.1 kHz, 48 kHz)
echo       x 4 multipliers (FS2, FS4, FS8, FS16)
echo       x 2 phases (linear, minimum)
echo       = 64 .npy files, ~6 GB total, hours of CPU.
echo       Skips files that already exist (restartable).
echo.
echo   [3] Show output directory contents
echo.
echo   [Q] Quit
echo.
echo ==============================================
set /p "CHOICE=  Select: "

if /i "%CHOICE%"=="1" goto interactive
if /i "%CHOICE%"=="2" goto allratios
if /i "%CHOICE%"=="3" goto listout
if /i "%CHOICE%"=="q" goto quit
echo.
echo   [!] Invalid choice. Press any key to retry...
pause >nul
goto menu

:interactive
echo.
echo ==============================================
echo   Interactive mode
echo ==============================================
echo.
%PY% optimize.py
echo.
echo ==============================================
echo  Done. Press any key to return to menu.
pause >nul
goto menu

:allratios
echo.
echo ==============================================
echo   All-Ratios Matrix Generation
echo ==============================================
echo.
echo   This will generate up to 64 .npy files in
echo   "%~dp0output\" — about 6 GB on disk.
echo   Files that already exist are skipped, so you
echo   can stop with Ctrl+C and resume later.
echo.
echo   Estimated time on a desktop CPU:
echo     1M  rows: a few minutes
echo     5M  rows: ~30 minutes
echo     10M rows: ~1 hour
echo     30M rows: ~4-8 hours
echo.
set /p "CONFIRM=  Continue? [Y/n]: "
if /i "%CONFIRM%"=="n" goto menu
echo.
%PY% optimize.py --all-ratios
echo.
echo ==============================================
echo  Done. Press any key to return to menu.
pause >nul
goto menu

:listout
echo.
echo ==============================================
echo   output\ directory contents
echo ==============================================
echo.
if exist "output\" (
    dir /b /o:n "output\*.npy" 2>nul
    echo.
    echo Total .npy files:
    dir /b "output\*.npy" 2>nul | find /c /v ""
    echo Total .npy size:
    for /f "tokens=3" %%s in ('dir /-c /a-d "output\*.npy" 2^>nul ^| findstr /c:" File("') do echo   %%s bytes
) else (
    echo   ^(no output directory yet — run option [1] or [2] first^)
)
echo.
pause
goto menu

:quit
endlocal
exit /b 0
