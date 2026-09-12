@echo off
setlocal
cd /d "%~dp0"

echo ===================================================
echo   Minecraft Auto-Enchanter Bot (DonutSMP)
echo ===================================================
echo.

if not exist ".env" (
    if exist ".env.example" (
        copy ".env.example" ".env" >nul
    ) else (
        echo # Add your Microsoft email including the @ symbol.> .env
        echo ACCOUNTS=>> .env
    )
    echo Created .env. Open it and set ACCOUNTS to your full Microsoft email, including @.
    echo Example: ACCOUNTS=your_email@outlook.com
    echo For multiple accounts, separate complete emails with commas.
    echo Save .env, then run this launcher again. No bot has been started.
    pause
    exit /b 1
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
