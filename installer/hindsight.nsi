Unicode true
!include "MUI2.nsh"
!include "x64.nsh"
!include "LogicLib.nsh"

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
  MessageBox MB_OKCANCEL "Quit hindsight from its tray menu before uninstalling; save any replay you want to keep first.$\r$\n$\r$\nUninstalling also removes hindsight's settings. Saved clips stay." /SD IDOK IDOK ready
  Abort
  ready:
FunctionEnd

# hindsight keeps per-user state outside $INSTDIR: its "Start with Windows" value in the user's
# Run key, and its settings, totals, log and buffer in that user's %LOCALAPPDATA%\Hindsight.
# The uninstaller is elevated, so HKCU and $LOCALAPPDATA here can belong to the administrator who
# approved the prompt rather than to the user who runs hindsight. Every logged-on user's hive is
# loaded under HKU, so this walks those instead and cleans each one; a user who is not logged on
# keeps a dead Run value until they next open Settings. Saved clips are the user's and stay.
Function un.RemoveUserState
  StrCpy $0 0
  ${Do}
    EnumRegKey $1 HKU "" $0
    ${If} $1 == ""
      ${ExitDo}
    ${EndIf}
    IntOp $0 $0 + 1
    StrCpy $2 $1 9
    StrCpy $3 $1 "" -8
    ${If} $2 == "S-1-5-21-"
    ${AndIf} $3 != "_Classes"
      DeleteRegValue HKU "$1\Software\Microsoft\Windows\CurrentVersion\Run" "hindsight"
      ReadRegStr $4 HKLM "SOFTWARE\Microsoft\Windows NT\CurrentVersion\ProfileList\$1" "ProfileImagePath"
      ${If} $4 != ""
        # The user's Local AppData folder, which is usually "%USERPROFILE%\AppData\Local" and
        # must be expanded against that user's profile, not this process's environment.
        ReadRegStr $5 HKU "$1\Software\Microsoft\Windows\CurrentVersion\Explorer\User Shell Folders" "Local AppData"
        StrCpy $6 $5 13
        ${If} $6 == "%USERPROFILE%"
          StrCpy $5 $5 "" 13
          StrCpy $5 "$4$5"
        ${ElseIf} $5 == ""
          StrCpy $5 "$4\AppData\Local"
        ${EndIf}
        ${If} $5 != ""
          RMDir /r "$5\Hindsight"
        ${EndIf}
      ${EndIf}
    ${EndIf}
  ${Loop}
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

  Call un.RemoveUserState
  DeleteRegKey HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\Hindsight"
SectionEnd
