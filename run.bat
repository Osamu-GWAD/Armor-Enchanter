@echo off
setlocal
cd /d "%~dp0"

echo ===================================================
echo   Minecraft Auto-Enchanter Bot (DonutSMP)
echo ===================================================
echo.

if not exist ".env" (
    if exist ".env.example" (
        echo [.env] not found. Copying .env.example to .env...
        copy ".env.example" ".env" >nul
        echo Please edit .env with your Microsoft email or Token, then press any key to continue.
        pause
    )
)

where cargo >nul 2>nul
if %ERRORLEVEL% EQU 0 (
    echo Starting Enchanter via Cargo in Release Mode...
    cargo run --release -- %*
) else if exist "target\release\Enchanter.exe" (
    echo Starting Enchanter from target\release\Enchanter.exe...
    target\release\Enchanter.exe %*
) else if exist "Enchanter.exe" (
    echo Starting Enchanter.exe...
    Enchanter.exe %*
) else (
    echo [ERROR] Neither Cargo nor Enchanter.exe was found!
    pause
)

if %ERRORLEVEL% NEQ 0 (
    echo.
    echo [ERROR] Enchanter exited with error code %ERRORLEVEL%.
    pause
)
