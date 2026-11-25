; Tawny for Windows: a per-user installer (no administrator rights).
;
;   makensis /DVERSION=0.2.0 /DEXE=path\to\tawny.exe /DOUT=Tawny-0.2.0-x64-setup.exe tawny.nsi
;
; It takes the place of the previous (CEF) Tawny cleanly: the same folder
; (%LOCALAPPDATA%\Tawny), the same Start menu entry and the same "Apps"
; entry, with the old program's files cleared out first. Tawny's data lives
; elsewhere (%LOCALAPPDATA%\io.github.PressF4me.Tawny) and is never touched
; here, so channels carry over and an uninstall keeps them.

Unicode true
SetCompressor /SOLID lzma
RequestExecutionLevel user
ManifestDPIAware true

!ifndef VERSION
  !error "VERSION is required"
!endif
!ifndef EXE
  !error "EXE (the built tawny.exe) is required"
!endif
!ifndef OUT
  !define OUT "Tawny-${VERSION}-x64-setup.exe"
!endif

!define APP "Tawny"
!define PUBLISHER "Tawny Pet Monitor"
!define UNINSTALL_KEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\${APP}"
!define URL_KEY "Software\Classes\tawny"

Name "${APP}"
OutFile "${OUT}"
InstallDir "$LOCALAPPDATA\${APP}"
Icon "tawny.ico"
UninstallIcon "tawny.ico"
VIProductVersion "${VERSION}.0"
VIAddVersionKey "ProductName" "${APP}"
VIAddVersionKey "FileDescription" "${APP} installer"
VIAddVersionKey "ProductVersion" "${VERSION}"
VIAddVersionKey "FileVersion" "${VERSION}"
VIAddVersionKey "CompanyName" "${PUBLISHER}"
VIAddVersionKey "LegalCopyright" "MIT"

!include "MUI2.nsh"
!include "LogicLib.nsh"

!define MUI_ICON "tawny.ico"
!define MUI_UNICON "tawny.ico"
!define MUI_FINISHPAGE_RUN "$INSTDIR\tawny.exe"
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH
!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES
!insertmacro MUI_LANGUAGE "English"
!insertmacro MUI_LANGUAGE "Spanish"

LangString Closing ${LANG_ENGLISH} "Closing Tawny…"
LangString Closing ${LANG_SPANISH} "Cerrando Tawny…"

; Tawny running holds tawny.exe: ask it to close (it saves as it goes), then
; make sure.
!macro CloseTawny
  DetailPrint "$(Closing)"
  nsExec::Exec 'taskkill /IM tawny.exe'
  Sleep 1500
  nsExec::Exec 'taskkill /F /IM tawny.exe'
  Sleep 500
!macroend

Section "Tawny"
  !insertmacro CloseTawny
  ; The previous Tawny's own files (the CEF runtime, its uninstaller): only
  ; ever in this, the app's own folder, and only when Tawny is what's there.
  ${If} ${FileExists} "$INSTDIR\tawny.exe"
    RMDir /r "$INSTDIR"
  ${EndIf}
  SetOutPath "$INSTDIR"
  File "/oname=tawny.exe" "${EXE}"
  File "tawny.ico"
  WriteUninstaller "$INSTDIR\uninstall.exe"

  CreateDirectory "$SMPROGRAMS\${APP}"
  CreateShortcut "$SMPROGRAMS\${APP}\${APP}.lnk" "$INSTDIR\tawny.exe"
  ; The previous version's desktop shortcut, if there is one, now opens this.
  ${If} ${FileExists} "$DESKTOP\${APP}.lnk"
    CreateShortcut "$DESKTOP\${APP}.lnk" "$INSTDIR\tawny.exe"
  ${EndIf}

  ; tawny:// pairing links open here (a later copy hands the link to the
  ; running one).
  WriteRegStr HKCU "${URL_KEY}" "" "URL:Tawny pairing link"
  WriteRegStr HKCU "${URL_KEY}" "URL Protocol" ""
  WriteRegStr HKCU "${URL_KEY}\DefaultIcon" "" '"$INSTDIR\tawny.exe",0'
  WriteRegStr HKCU "${URL_KEY}\shell\open\command" "" '"$INSTDIR\tawny.exe" "%1"'

  WriteRegStr HKCU "${UNINSTALL_KEY}" "DisplayName" "${APP}"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "DisplayVersion" "${VERSION}"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "Publisher" "${PUBLISHER}"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "DisplayIcon" '"$INSTDIR\tawny.exe",0'
  WriteRegStr HKCU "${UNINSTALL_KEY}" "InstallLocation" "$INSTDIR"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "UninstallString" '"$INSTDIR\uninstall.exe"'
  WriteRegStr HKCU "${UNINSTALL_KEY}" "QuietUninstallString" '"$INSTDIR\uninstall.exe" /S'
  WriteRegStr HKCU "${UNINSTALL_KEY}" "URLInfoAbout" "https://github.com/Tawny-Pet-Monitor/Tawny-Desktop"
  WriteRegDWORD HKCU "${UNINSTALL_KEY}" "NoModify" 1
  WriteRegDWORD HKCU "${UNINSTALL_KEY}" "NoRepair" 1
SectionEnd

Section "Uninstall"
  !insertmacro CloseTawny
  Delete "$INSTDIR\tawny.exe"
  Delete "$INSTDIR\tawny.ico"
  Delete "$INSTDIR\uninstall.exe"
  RMDir "$INSTDIR"
  Delete "$SMPROGRAMS\${APP}\${APP}.lnk"
  RMDir "$SMPROGRAMS\${APP}"
  ; A desktop shortcut only if it is ours.
  ${If} ${FileExists} "$DESKTOP\${APP}.lnk"
    Delete "$DESKTOP\${APP}.lnk"
  ${EndIf}
  ; The link handler only while it still points here.
  ReadRegStr $0 HKCU "${URL_KEY}\shell\open\command" ""
  ${If} $0 == '"$INSTDIR\tawny.exe" "%1"'
    DeleteRegKey HKCU "${URL_KEY}"
  ${EndIf}
  DeleteRegKey HKCU "${UNINSTALL_KEY}"
  ; Channels and settings (%LOCALAPPDATA%\io.github.PressF4me.Tawny) stay.
SectionEnd
