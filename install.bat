@echo off
chcp 65001 >nul
setlocal
echo ============================================
echo   TFM2 드래프트 추천기 설치
echo ============================================
echo.

REM 1) 오버레이 → %LOCALAPPDATA%\tfm2-overlay
set "DEST=%LOCALAPPDATA%\tfm2-overlay"
if not exist "%DEST%" mkdir "%DEST%"
if not exist "%DEST%\stats" mkdir "%DEST%\stats"
copy /Y "%~dp0release\overlay\*" "%DEST%\" >nul
echo [1/2] 오버레이 설치 완료
echo       %DEST%
echo.

REM 2) 모드 → 게임 mods 폴더
set "GAME=C:\Program Files (x86)\Steam\steamapps\common\Teamfight Manager2"
if exist "%GAME%\mods" (
  xcopy /Y /E /I "%~dp0release\mod\tfm2_db_export" "%GAME%\mods\tfm2_db_export" >nul
  echo [2/2] 모드 설치 완료
  echo       %GAME%\mods\tfm2_db_export
) else (
  echo [2/2] 게임을 기본 경로에서 못 찾았습니다.
  echo       아래 폴더를 직접 복사하세요:
  echo         FROM: %~dp0release\mod\tfm2_db_export
  echo         TO:   (게임 설치폴더)\mods\tfm2_db_export
)
echo.
echo ============================================
echo   설치 끝!
echo   1) 게임 실행
echo   2) Mods 메뉴에서 "TFM2 추천기 통계 추출기" 켜기
echo   3) 게임 재시작 → 세이브 로드하면 오버레이 자동 실행
echo      (Ctrl+F10 으로 켜기/끄기)
echo ============================================
echo.
pause
