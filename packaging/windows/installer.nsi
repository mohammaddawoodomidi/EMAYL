; EMAYL Windows installer (NSIS 3).
;
; Built by the release workflow with:
;   makensis /DVERSION=0.1.0 /DSOURCE_EXE=<path to emayl.exe> /DOUTFILE=<installer file> installer.nsi
;
; Installs for the current user only: no administrator prompt, nothing outside the
; user's own folders. Upgrading is just running a newer installer over the old one.

Unicode true
SetCompressor /SOLID lzma
ManifestDPIAware true
ManifestSupportedOS Win10
RequestExecutionLevel user

!ifndef VERSION
  !error "Pass /DVERSION=x.y.z"
!endif
!ifndef SOURCE_EXE
  !error "Pass /DSOURCE_EXE=path\to\emayl.exe"
!endif
!ifndef OUTFILE
  !define OUTFILE "emayl-for-windows.exe"
!endif

!define APP "EMAYL"
!define APPKEY "Software\EMAYL"
!define UNINSTKEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\EMAYL"

!include "MUI2.nsh"
!include "LogicLib.nsh"
!include "FileFunc.nsh"

Name "${APP}"
OutFile "${OUTFILE}"
InstallDir "$LOCALAPPDATA\Programs\${APP}"
InstallDirRegKey HKCU "${APPKEY}" "InstallDir"
BrandingText "${APP} ${VERSION}"

VIProductVersion "${VERSION}.0"
VIAddVersionKey "ProductName" "${APP}"
VIAddVersionKey "CompanyName" "EMAYL"
VIAddVersionKey "FileDescription" "${APP} installer"
VIAddVersionKey "FileVersion" "${VERSION}"
VIAddVersionKey "ProductVersion" "${VERSION}"
VIAddVersionKey "LegalCopyright" "Copyright (c) EMAYL contributors"

!define MUI_ICON "..\..\assets\icon\emayl.ico"
!define MUI_UNICON "..\..\assets\icon\emayl.ico"
!define MUI_WELCOMEFINISHPAGE_BITMAP "sidebar.bmp"
!define MUI_UNWELCOMEFINISHPAGE_BITMAP "sidebar.bmp"
!define MUI_HEADERIMAGE
!define MUI_HEADERIMAGE_BITMAP "header.bmp"
!define MUI_HEADERIMAGE_RIGHT
!define MUI_ABORTWARNING

!define MUI_WELCOMEPAGE_TITLE "Welcome to ${APP}"
!define MUI_WELCOMEPAGE_TEXT "${APP} is a fast, private email program for all your accounts.$\r$\n$\r$\nThis installs version ${VERSION} for your Windows user only. No administrator rights are needed.$\r$\n$\r$\nClick Next to continue."
!define MUI_FINISHPAGE_TITLE "${APP} is installed"
!define MUI_FINISHPAGE_RUN "$INSTDIR\emayl.exe"
!define MUI_FINISHPAGE_RUN_TEXT "Start ${APP}"
!define MUI_FINISHPAGE_SHOWREADME ""
!define MUI_FINISHPAGE_SHOWREADME_TEXT "Add a shortcut to the desktop"
!define MUI_FINISHPAGE_SHOWREADME_FUNCTION CreateDesktopShortcut
!define MUI_FINISHPAGE_SHOWREADME_NOTCHECKED
!define MUI_UNCONFIRMPAGE_TEXT_TOP "${APP} will be removed from this computer. Your mail accounts and messages are not touched on the mail servers."

!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH

!insertmacro MUI_UNPAGE_WELCOME
!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES
!insertmacro MUI_UNPAGE_FINISH

!insertmacro MUI_LANGUAGE "English"

; Replacing a program file that is open fails, and closing the program for the
; person could lose a half-written message. So ask them to close it themselves.
!macro CheckNotRunning PREFIX
  Function ${PREFIX}CheckNotRunning
    ${If} ${FileExists} "$INSTDIR\emayl.exe"
      ${Do}
        ClearErrors
        FileOpen $0 "$INSTDIR\emayl.exe" a
        ${IfNot} ${Errors}
          FileClose $0
          ${Break}
        ${EndIf}
        MessageBox MB_RETRYCANCEL|MB_ICONEXCLAMATION "${APP} is open. Please close it, then choose Retry." /SD IDCANCEL IDRETRY +2
        Abort
      ${Loop}
    ${EndIf}
  FunctionEnd
!macroend
!insertmacro CheckNotRunning ""
!insertmacro CheckNotRunning "un."

Function .onInit
  Call CheckNotRunning
FunctionEnd

Function CreateDesktopShortcut
  CreateShortCut "$DESKTOP\${APP}.lnk" "$INSTDIR\emayl.exe" "" "$INSTDIR\emayl.exe" 0
FunctionEnd

Section "Install"
  SetOutPath "$INSTDIR"
  ; The folder may have been changed on the directory page.
  Call CheckNotRunning
  File "/oname=emayl.exe" "${SOURCE_EXE}"
  File "..\..\LICENSE"
  File "..\..\README.md"
  WriteUninstaller "$INSTDIR\Uninstall.exe"

  CreateShortCut "$SMPROGRAMS\${APP}.lnk" "$INSTDIR\emayl.exe" "" "$INSTDIR\emayl.exe" 0

  WriteRegStr HKCU "${APPKEY}" "InstallDir" "$INSTDIR"
  WriteRegStr HKCU "${UNINSTKEY}" "DisplayName" "${APP}"
  WriteRegStr HKCU "${UNINSTKEY}" "DisplayVersion" "${VERSION}"
  WriteRegStr HKCU "${UNINSTKEY}" "Publisher" "EMAYL"
  WriteRegStr HKCU "${UNINSTKEY}" "DisplayIcon" "$INSTDIR\emayl.exe,0"
  WriteRegStr HKCU "${UNINSTKEY}" "InstallLocation" "$INSTDIR"
  WriteRegStr HKCU "${UNINSTKEY}" "UninstallString" '"$INSTDIR\Uninstall.exe"'
  WriteRegStr HKCU "${UNINSTKEY}" "QuietUninstallString" '"$INSTDIR\Uninstall.exe" /S'
  WriteRegDWORD HKCU "${UNINSTKEY}" "NoModify" 1
  WriteRegDWORD HKCU "${UNINSTKEY}" "NoRepair" 1
  ${GetSize} "$INSTDIR" "/S=0K" $0 $1 $2
  WriteRegDWORD HKCU "${UNINSTKEY}" "EstimatedSize" $0
SectionEnd

Section "Uninstall"
  Call un.CheckNotRunning

  ; Only the files this installer put there are removed, so a folder the person
  ; shares with something else is left alone.
  Delete "$INSTDIR\emayl.exe"
  Delete "$INSTDIR\LICENSE"
  Delete "$INSTDIR\README.md"
  Delete "$INSTDIR\Uninstall.exe"
  RMDir "$INSTDIR"

  Delete "$SMPROGRAMS\${APP}.lnk"
  Delete "$DESKTOP\${APP}.lnk"
  DeleteRegKey HKCU "${UNINSTKEY}"
  DeleteRegKey HKCU "${APPKEY}"

  ; The mail kept on this computer is the person's. It is only deleted if they say so.
  MessageBox MB_YESNO|MB_ICONQUESTION|MB_DEFBUTTON2 "Also delete the saved copy of your mail and your settings from this computer?$\r$\n$\r$\nChoose No to keep them, for example if you plan to install ${APP} again. Nothing on your mail servers is changed either way.$\r$\n$\r$\nPasswords are kept by Windows in its Credential Manager. To delete those too, remove each account inside ${APP} before uninstalling." /SD IDNO IDNO keep_data
    RMDir /r "$APPDATA\emayl\EMAYL"
    RMDir "$APPDATA\emayl"
  keep_data:
SectionEnd
