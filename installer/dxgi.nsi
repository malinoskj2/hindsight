Unicode true
!include "MUI2.nsh"
!include "x64.nsh"
Name "dxgi"
OutFile "..\dist\dxgi-setup-x64.exe"
InstallDir "$PROGRAMFILES64\dxgi"
RequestExecutionLevel admin
SetCompressor /SOLID lzma
!define MUI_ABORTWARNING
!define MUI_ICON "..\assets\dxgi.ico"
!define MUI_UNICON "..\assets\dxgi.ico"
!define MUI_FINISHPAGE_RUN "$INSTDIR\dxgi.exe"
!define MUI_FINISHPAGE_RUN_TEXT "Launch dxgi now"
!define MUI_FINISHPAGE_RUN_FUNCTION LaunchApplication
!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH
!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES
!insertmacro MUI_LANGUAGE "English"

# The installer is elevated; start the app as the logged-on user so it sees that user's
# desktop and tray.
Function LaunchApplication
  Exec '"$WINDIR\explorer.exe" "$INSTDIR\dxgi.exe"'
FunctionEnd

Function .onInit
  ${IfNot} ${RunningX64}
    MessageBox MB_ICONSTOP "dxgi requires 64-bit Windows."
    Abort
  ${EndIf}

  SetRegView 64
  SetShellVarContext all
FunctionEnd

Section "dxgi"
  SetOutPath "$INSTDIR"
  File "..\dist\dxgi.exe"
  File "..\dist\README.md"

  WriteUninstaller "$INSTDIR\uninstall.exe"
  CreateDirectory "$SMPROGRAMS\dxgi"
  CreateShortcut "$SMPROGRAMS\dxgi\dxgi.lnk" "$INSTDIR\dxgi.exe" "" "$INSTDIR\dxgi.exe" 0
  CreateShortcut "$SMPROGRAMS\dxgi\Uninstall.lnk" "$INSTDIR\uninstall.exe"

  WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\Dxgi" "DisplayIcon" "$INSTDIR\dxgi.exe"
  WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\Dxgi" "DisplayName" "dxgi"
  WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\Dxgi" "DisplayVersion" "0.1.0"
  WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\Dxgi" "UninstallString" '"$INSTDIR\uninstall.exe"'
  WriteRegDWORD HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\Dxgi" "NoModify" 1
  WriteRegDWORD HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\Dxgi" "NoRepair" 1
SectionEnd

Function un.onInit
  SetRegView 64
  SetShellVarContext all
  MessageBox MB_OKCANCEL "Quit dxgi from its tray menu before uninstalling; save any replay you want to keep first." IDOK ready
  Abort
  ready:
FunctionEnd

Section "Uninstall"
  Delete "$SMPROGRAMS\dxgi\dxgi.lnk"
  Delete "$SMPROGRAMS\dxgi\Uninstall.lnk"
  RMDir "$SMPROGRAMS\dxgi"

  Delete "$INSTDIR\dxgi.exe"
  Delete "$INSTDIR\README.md"
  Delete "$INSTDIR\uninstall.exe"
  RMDir "$INSTDIR"

  DeleteRegKey HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\Dxgi"
SectionEnd
