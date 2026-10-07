; Telinha installer for Windows (NSIS, Modern UI 2).
; Installs for the current user only: does not ask for administrator rights.
; Built by scripts/package.sh, which defines VERSION and STAGE.

Unicode true
!include "MUI2.nsh"

Name "Telinha"
OutFile "${OUTFILE}"
InstallDir "$LOCALAPPDATA\Programs\Telinha"
InstallDirRegKey HKCU "Software\Telinha" "InstallDir"
RequestExecutionLevel user
SetCompressor /SOLID lzma

!define MUI_ICON "..\icons\telinha.ico"
!define MUI_UNICON "..\icons\telinha.ico"
!define MUI_ABORTWARNING
!define MUI_FINISHPAGE_RUN "$INSTDIR\telinha.exe"
!define MUI_FINISHPAGE_RUN_TEXT "Abrir o Telinha agora"

!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH
!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES
!insertmacro MUI_LANGUAGE "PortugueseBR"

VIProductVersion "${VERSION}.0"
VIAddVersionKey /LANG=${LANG_PORTUGUESEBR} "ProductName" "Telinha"
VIAddVersionKey /LANG=${LANG_PORTUGUESEBR} "FileDescription" "Instalador do Telinha"
VIAddVersionKey /LANG=${LANG_PORTUGUESEBR} "FileVersion" "${VERSION}"
VIAddVersionKey /LANG=${LANG_PORTUGUESEBR} "ProductVersion" "${VERSION}"

Section "Telinha"
  SetOutPath "$INSTDIR"
  ; Closes a running copy before replacing the files.
  nsExec::Exec 'taskkill /IM telinha.exe /F'
  File "${STAGE}\*.exe"
  File "${STAGE}\*.dll"
  WriteRegStr HKCU "Software\Telinha" "InstallDir" "$INSTDIR"
  WriteUninstaller "$INSTDIR\Desinstalar.exe"
  CreateDirectory "$SMPROGRAMS\Telinha"
  CreateShortcut "$SMPROGRAMS\Telinha\Telinha.lnk" "$INSTDIR\telinha.exe"
  CreateShortcut "$DESKTOP\Telinha.lnk" "$INSTDIR\telinha.exe"
  ; Shows up in "Installed apps" for uninstalling.
  !define UNINST "Software\Microsoft\Windows\CurrentVersion\Uninstall\Telinha"
  WriteRegStr HKCU "${UNINST}" "DisplayName" "Telinha"
  WriteRegStr HKCU "${UNINST}" "DisplayVersion" "${VERSION}"
  WriteRegStr HKCU "${UNINST}" "Publisher" "Telinha"
  WriteRegStr HKCU "${UNINST}" "DisplayIcon" "$INSTDIR\telinha.exe"
  WriteRegStr HKCU "${UNINST}" "UninstallString" '"$INSTDIR\Desinstalar.exe"'
  WriteRegDWORD HKCU "${UNINST}" "NoModify" 1
  WriteRegDWORD HKCU "${UNINST}" "NoRepair" 1
SectionEnd

Section "Uninstall"
  nsExec::Exec 'taskkill /IM telinha.exe /F'
  Delete "$INSTDIR\*.exe"
  Delete "$INSTDIR\*.dll"
  RMDir "$INSTDIR"
  Delete "$SMPROGRAMS\Telinha\Telinha.lnk"
  RMDir "$SMPROGRAMS\Telinha"
  Delete "$DESKTOP\Telinha.lnk"
  DeleteRegKey HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\Telinha"
  DeleteRegKey HKCU "Software\Telinha"
SectionEnd
