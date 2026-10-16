Unicode true
!include "MUI2.nsh"
!include "x64.nsh"

# The version comes from Cargo.toml, passed in by scripts\build-windows.ps1, so that the installer
# cannot drift from the program it installs.
!ifndef VERSION
  !error "VERSION is not defined; build with scripts\build-windows.ps1, which passes /DVERSION from Cargo.toml"
!endif
!ifndef VERSION_NUMERIC
  !error "VERSION_NUMERIC is not defined; build with scripts\build-windows.ps1, which passes /DVERSION_NUMERIC from Cargo.toml"
!endif

Name "hindsight"
OutFile "..\dist\hindsight-setup-x64.exe"
InstallDir "$PROGRAMFILES64\hindsight"
RequestExecutionLevel admin
SetCompressor /SOLID lzma
!define MUI_ABORTWARNING
!define MUI_ICON "..\assets\hindsight.ico"
!define MUI_UNICON "..\assets\hindsight.ico"
!define MUI_FINISHPAGE_RUN "$INSTDIR\hindsight.exe"
!define MUI_FINISHPAGE_RUN_TEXT "Launch hindsight now"
!define MUI_FINISHPAGE_RUN_FUNCTION LaunchApplication
!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_LICENSE "..\LICENSE"
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH
!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES
!insertmacro MUI_LANGUAGE "English"

# The installer's own file properties.
VIProductVersion "${VERSION_NUMERIC}"
VIAddVersionKey "ProductName" "hindsight"
VIAddVersionKey "ProductVersion" "${VERSION}"
VIAddVersionKey "FileVersion" "${VERSION}"
VIAddVersionKey "FileDescription" "hindsight installer"
VIAddVersionKey "LegalCopyright" "Copyright (c) 2026, Jesse Malinosky"

# The installer is elevated; start the app as the logged-on user so it sees that user's
# desktop and tray.
Function LaunchApplication
  Exec '"$WINDIR\explorer.exe" "$INSTDIR\hindsight.exe"'
FunctionEnd

Function .onInit
  ${IfNot} ${RunningX64}
    MessageBox MB_ICONSTOP "hindsight requires 64-bit Windows."
    Abort
  ${EndIf}

  SetRegView 64
  SetShellVarContext all
FunctionEnd

Section "hindsight"
  SetOutPath "$INSTDIR"
  File "..\dist\hindsight.exe"
  File "..\dist\README.md"
  File "..\dist\LICENSE"

  WriteUninstaller "$INSTDIR\uninstall.exe"
  CreateDirectory "$SMPROGRAMS\hindsight"
  CreateShortcut "$SMPROGRAMS\hindsight\hindsight.lnk" "$INSTDIR\hindsight.exe" "" "$INSTDIR\hindsight.exe" 0
  CreateShortcut "$SMPROGRAMS\hindsight\Uninstall.lnk" "$INSTDIR\uninstall.exe"

  WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\Hindsight" "DisplayIcon" "$INSTDIR\hindsight.exe"
  WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\Hindsight" "DisplayName" "hindsight"
  WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\Hindsight" "DisplayVersion" "${VERSION}"
  WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\Hindsight" "UninstallString" '"$INSTDIR\uninstall.exe"'
  WriteRegDWORD HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\Hindsight" "NoModify" 1
  WriteRegDWORD HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\Hindsight" "NoRepair" 1
SectionEnd

Function un.onInit
  SetRegView 64
  SetShellVarContext all
  MessageBox MB_OKCANCEL "Quit hindsight from its tray menu before uninstalling; save any replay you want to keep first." IDOK ready
  Abort
  ready:
FunctionEnd

Section "Uninstall"
  Delete "$SMPROGRAMS\hindsight\hindsight.lnk"
  Delete "$SMPROGRAMS\hindsight\Uninstall.lnk"
  RMDir "$SMPROGRAMS\hindsight"

  Delete "$INSTDIR\hindsight.exe"
  Delete "$INSTDIR\README.md"
  Delete "$INSTDIR\LICENSE"
  Delete "$INSTDIR\uninstall.exe"
  RMDir "$INSTDIR"

  DeleteRegKey HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\Hindsight"
SectionEnd
