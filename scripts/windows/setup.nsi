; Apparition's setup for Windows: Apparition-Setup-<version>-windows-<arch>.exe.
;
; Built by scripts/package-release.sh with makensis (NSIS 3), from the same three files the zip
; holds:
;
;   makensis -DVERSION=1.0.0 -DARCH=x64 -DSOURCE=<folder> -DOUTFILE=<setup.exe> -DICON=<icon.ico>
;            [-DLOADER] scripts/windows/setup.nsi        (-DLOADER: the folder has WebView2Loader.dll)
;
; What it is for. Windows Defender Firewall's rules name a program by where its file is, and
; making one takes an administrator. A copy unpacked from the zip gets its rule from Windows'
; own question the first time it listens, which nobody sees when it is started at login or over
; SSH. Setup puts the files where they stay and makes the rules itself, so an installed copy
; never meets the question; removing it takes them away.
;
; It asks one thing: who Apparition is for.
;
;   Only me       %LOCALAPPDATA%\Programs\Apparition. Nothing but the firewall rules needs an
;                 administrator (one approval), and the app keeps itself up to date without one.
;   Everyone      %ProgramFiles%\Apparition, and the companion as a Windows service, for a PC
;                 that should serve its devices with nobody signed in. All of setup then runs as
;                 an administrator, and so does each update's file swap.
;
; Setup starts as the person, not as an administrator, so that it knows whose Apparition folder
; the service is to use: on a PC where approving means typing another account's password, a
; setup that started elevated would see that account's folders. For "Everyone" it starts itself
; again elevated and hands that folder over (/COMPANION).
;
; The work on the system (the rules, the service, PATH) is done by apparition-cli.exe, which a
; copy from the zip has too: `firewall`, `service` and `link` in installer/src/windows.rs.
;
; Without its pages (for scripts):  Apparition-Setup.exe /S [/SYSTEM] [/PATH] [/D=<folder>]
;   /S       no pages. /SYSTEM needs a prompt that is already an administrator's.
;   /SYSTEM  for everyone, with the service.
;   /PATH    put apparition-cli on PATH.
;   /D=      another folder (last, unquoted, as NSIS takes it).
; The NSIS program in front is 32-bit x86 on every PC, which Windows on ARM runs too; what it
; installs is native.

Unicode true
ManifestDPIAware true
RequestExecutionLevel user
SetCompressor /SOLID lzma

!include "MUI2.nsh"
!include "nsDialogs.nsh"
!include "LogicLib.nsh"
!include "FileFunc.nsh"
!include "StrFunc.nsh"
!include "x64.nsh"
${UnStrStr}

!ifndef VERSION | ARCH | SOURCE | OUTFILE | ICON
  !error "setup.nsi needs -DVERSION, -DARCH, -DSOURCE, -DOUTFILE and -DICON (see its first lines)"
!endif

!define UNINSTALL "Software\Microsoft\Windows\CurrentVersion\Uninstall\Apparition"
!define AUTOSTART "Software\Microsoft\Windows\CurrentVersion\Run"

Name "Apparition"
OutFile "${OUTFILE}"
BrandingText "Apparition ${VERSION} (${ARCH})"
InstallDir ""
ShowInstDetails show
ShowUninstDetails show

Var Mode          ; "user" or "system"
Var Chosen        ; 1 when /SYSTEM said so and the page isn't shown
Var CompanionDir  ; the person's companion folder, which the service uses
Var AddToPath
Var Person        ; who started setup, when it was started again as an administrator
Var ForMe
Var ForEveryone
Var PathBox

!define MUI_ICON "${ICON}"
!define MUI_UNICON "${ICON}"
!define MUI_ABORTWARNING

Page custom ModePage ModePageLeave
!insertmacro MUI_PAGE_INSTFILES
!define MUI_FINISHPAGE_RUN
!define MUI_FINISHPAGE_RUN_TEXT "Open Apparition"
!define MUI_FINISHPAGE_RUN_FUNCTION OpenApp
!define MUI_PAGE_CUSTOMFUNCTION_SHOW FinishShow
!insertmacro MUI_PAGE_FINISH

!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES

!insertmacro MUI_LANGUAGE "English"

; Whether this process is an administrator's, elevated: $0 is "Admin" then.
!macro AccountType
  UserInfo::GetAccountType
  Pop $0
!macroend

; Stops until the app's file can be replaced or removed: Windows won't touch a running program.
!macro WaitForApp
  ${Do}
    ${IfNot} ${FileExists} "$INSTDIR\Apparition.exe"
      ${Break}
    ${EndIf}
    ClearErrors
    FileOpen $0 "$INSTDIR\Apparition.exe" a
    ${IfNot} ${Errors}
      FileClose $0
      ${Break}
    ${EndIf}
    MessageBox MB_RETRYCANCEL|MB_ICONEXCLAMATION "Apparition is running. Quit it (its icon by the clock, then Quit) and choose Retry." /SD IDCANCEL IDRETRY +2
    Abort "Apparition is running; nothing was changed."
  ${Loop}
!macroend

Function .onInit
  SetRegView 64
  ${IfNot} ${RunningX64}
    MessageBox MB_ICONSTOP "Apparition needs 64-bit Windows 10 or 11." /SD IDOK
    Abort
  ${EndIf}
  !if "${ARCH}" == "arm64"
    ${IfNot} ${IsNativeARM64}
      MessageBox MB_ICONSTOP "This is Apparition for Windows on ARM. This PC has an Intel or AMD processor: download the x64 setup instead." /SD IDOK
      Abort
    ${EndIf}
  !endif

  StrCpy $Mode "user"
  StrCpy $Chosen 0
  StrCpy $AddToPath 0
  StrCpy $CompanionDir "$APPDATA\Apparition Installer\anisette"
  StrCpy $Person ""
  ${GetParameters} $R0
  ClearErrors
  ${GetOptions} $R0 "/SYSTEM" $R1
  ${IfNot} ${Errors}
    StrCpy $Mode "system"
    StrCpy $Chosen 1
  ${EndIf}
  ClearErrors
  ${GetOptions} $R0 "/PATH" $R1
  ${IfNot} ${Errors}
    StrCpy $AddToPath 1
  ${EndIf}
  ClearErrors
  ${GetOptions} $R0 "/COMPANION=" $R1
  ${IfNot} ${Errors}
    StrCpy $CompanionDir $R1
  ${EndIf}
  ClearErrors
  ${GetOptions} $R0 "/PERSON=" $R1
  ${IfNot} ${Errors}
    StrCpy $Person $R1
  ${EndIf}
  ClearErrors

  ${If} $Mode == "system"
    !insertmacro AccountType
    ${If} $0 != "Admin"
      MessageBox MB_ICONSTOP "Setting Apparition up for everyone takes an administrator. Open setup without /SYSTEM and choose it there, or run this from an administrator's prompt." /SD IDOK
      Abort
    ${EndIf}
  ${EndIf}
FunctionEnd

Function ModePage
  ${If} $Chosen == 1
    Abort
  ${EndIf}
  !insertmacro MUI_HEADER_TEXT "Who is Apparition for?" "Either way, setup lets your devices through Windows Defender Firewall."
  nsDialogs::Create 1018
  Pop $0

  ${NSD_CreateRadioButton} 0 0 100% 12u "Only me"
  Pop $ForMe
  ${NSD_CreateLabel} 12u 14u 96% 28u "Apparition runs while you are signed in, and keeps itself up to date without asking. Windows asks an administrator's approval once now, for the firewall."
  Pop $0

  ${NSD_CreateRadioButton} 0 50u 100% 12u "Everyone on this PC, with a background service"
  Pop $ForEveryone
  ${NSD_CreateLabel} 12u 64u 96% 38u "Also serves your devices with nobody signed in: the relay, and the anisette you choose in the app, run as a Windows service using your pairings. Setup and each update need an administrator's approval."
  Pop $0

  ${NSD_CreateCheckbox} 0 114u 100% 12u "Make the command line (apparition-cli) available in every terminal"
  Pop $PathBox

  ${If} $Mode == "system"
    ${NSD_Check} $ForEveryone
  ${Else}
    ${NSD_Check} $ForMe
  ${EndIf}
  ${If} $AddToPath == 1
    ${NSD_Check} $PathBox
  ${EndIf}
  nsDialogs::Show
FunctionEnd

Function ModePageLeave
  ${NSD_GetState} $ForEveryone $0
  ${If} $0 == ${BST_CHECKED}
    StrCpy $Mode "system"
  ${Else}
    StrCpy $Mode "user"
  ${EndIf}
  ${NSD_GetState} $PathBox $AddToPath
  ${If} $Mode == "system"
    !insertmacro AccountType
    ${If} $0 != "Admin"
      ; Again as an administrator, told whose companion folder the service is to use.
      UserInfo::GetName
      Pop $1
      StrCpy $2 '/SYSTEM /COMPANION="$CompanionDir" /PERSON="$1"'
      ${If} $AddToPath == 1
        StrCpy $2 "$2 /PATH"
      ${EndIf}
      ClearErrors
      ExecShell "runas" "$EXEPATH" "$2"
      ${If} ${Errors}
        MessageBox MB_ICONEXCLAMATION "Windows didn't get an administrator's approval, which setting Apparition up for everyone needs. Try again, or choose Only me."
        Abort
      ${EndIf}
      Quit
    ${EndIf}
  ${EndIf}
FunctionEnd

Section "Apparition"
  SetRegView 64
  ${If} $Mode == "system"
    SetShellVarContext all
    ${If} $INSTDIR == ""
      StrCpy $INSTDIR "$PROGRAMFILES64\Apparition"
    ${EndIf}
  ${Else}
    SetShellVarContext current
    ${If} $INSTDIR == ""
      StrCpy $INSTDIR "$LOCALAPPDATA\Programs\Apparition"
    ${EndIf}
  ${EndIf}

  ; A copy already here: its service holds apparition-cli.exe, and the app its own file.
  ${If} ${FileExists} "$INSTDIR\apparition-cli.exe"
    DetailPrint "Stopping the Apparition service, if this copy has one..."
    nsExec::Exec '"$INSTDIR\apparition-cli.exe" service stop'
    Pop $0
  ${EndIf}
  !insertmacro WaitForApp

  SetOutPath "$INSTDIR"
  File "${SOURCE}\Apparition.exe"
  File "${SOURCE}\apparition-cli.exe"
  !ifdef LOADER
    File "${SOURCE}\WebView2Loader.dll"
  !endif
  WriteUninstaller "$INSTDIR\uninstall.exe"
  CreateShortcut "$SMPROGRAMS\Apparition.lnk" "$INSTDIR\Apparition.exe"

  WriteRegStr SHCTX "${UNINSTALL}" "DisplayName" "Apparition"
  WriteRegStr SHCTX "${UNINSTALL}" "DisplayVersion" "${VERSION}"
  WriteRegStr SHCTX "${UNINSTALL}" "Publisher" "Illixion"
  WriteRegStr SHCTX "${UNINSTALL}" "URLInfoAbout" "https://apparition.illixion.com"
  WriteRegStr SHCTX "${UNINSTALL}" "DisplayIcon" "$INSTDIR\Apparition.exe"
  WriteRegStr SHCTX "${UNINSTALL}" "InstallLocation" "$INSTDIR"
  WriteRegStr SHCTX "${UNINSTALL}" "UninstallString" '"$INSTDIR\uninstall.exe"'
  WriteRegStr SHCTX "${UNINSTALL}" "QuietUninstallString" '"$INSTDIR\uninstall.exe" /S'
  WriteRegDWORD SHCTX "${UNINSTALL}" "NoModify" 1
  WriteRegDWORD SHCTX "${UNINSTALL}" "NoRepair" 1
  ${GetSize} "$INSTDIR" "/S=0K" $0 $1 $2
  IntFmt $0 "0x%08X" $0
  WriteRegDWORD SHCTX "${UNINSTALL}" "EstimatedSize" "$0"

  DetailPrint "Letting your devices through Windows Defender Firewall (Windows may ask you to approve)..."
  nsExec::ExecToLog '"$INSTDIR\apparition-cli.exe" firewall allow "$INSTDIR\Apparition.exe" "$INSTDIR\apparition-cli.exe"'
  Pop $0
  ${If} $0 != 0
    DetailPrint "The firewall wasn't changed. Windows asks instead when Apparition first listens: allow it there."
  ${EndIf}

  ${If} $Mode == "system"
    DetailPrint "Installing the Apparition service..."
    nsExec::ExecToLog '"$INSTDIR\apparition-cli.exe" service install --data-dir "$CompanionDir"'
    Pop $0
    ${If} $0 != 0
      MessageBox MB_ICONEXCLAMATION "The Apparition service couldn't be installed, so this PC serves your devices only while the app is open. The details are in the list behind this message." /SD IDOK
      SetErrorLevel 2
    ${EndIf}
  ${EndIf}

  ${If} $AddToPath == 1
    ${If} $Mode == "system"
      nsExec::ExecToLog '"$INSTDIR\apparition-cli.exe" link --system'
    ${Else}
      nsExec::ExecToLog '"$INSTDIR\apparition-cli.exe" link'
    ${EndIf}
    Pop $0
  ${EndIf}
SectionEnd

; Setup for everyone runs as an administrator. Explorer starts the app as the person whose
; desktop this is; when another account approved, even that would start it as the wrong one, so
; the offer isn't made.
Function FinishShow
  ${If} $Person != ""
    UserInfo::GetName
    Pop $0
    ${If} $0 != $Person
      SendMessage $mui.FinishPage.Run ${BM_SETCHECK} ${BST_UNCHECKED} 0
      ShowWindow $mui.FinishPage.Run ${SW_HIDE}
    ${EndIf}
  ${EndIf}
FunctionEnd

Function OpenApp
  ${If} $Mode == "system"
    Exec '"$WINDIR\explorer.exe" "$INSTDIR\Apparition.exe"'
  ${Else}
    Exec '"$INSTDIR\Apparition.exe"'
  ${EndIf}
FunctionEnd

; Removing. A copy for everyone is listed under the machine, and takes an administrator.
Function un.onInit
  SetRegView 64
  StrCpy $Mode "user"
  ReadRegStr $0 HKLM "${UNINSTALL}" "InstallLocation"
  ${If} $0 == $INSTDIR
    StrCpy $Mode "system"
    !insertmacro AccountType
    ${If} $0 != "Admin"
      ${If} ${Silent}
        Abort
      ${EndIf}
      ClearErrors
      ExecShell "runas" "$INSTDIR\uninstall.exe"
      ${If} ${Errors}
        MessageBox MB_ICONEXCLAMATION "Removing a copy of Apparition that was set up for everyone takes an administrator's approval."
      ${EndIf}
      Quit
    ${EndIf}
  ${EndIf}
FunctionEnd

Section "Uninstall"
  SetRegView 64
  ${If} $Mode == "system"
    SetShellVarContext all
  ${Else}
    SetShellVarContext current
  ${EndIf}

  ${If} $Mode == "system"
    DetailPrint "Removing the Apparition service..."
    nsExec::ExecToLog '"$INSTDIR\apparition-cli.exe" service uninstall'
    Pop $0
  ${EndIf}
  !insertmacro WaitForApp

  DetailPrint "Taking Apparition's rules out of Windows Defender Firewall (Windows may ask you to approve)..."
  nsExec::ExecToLog '"$INSTDIR\apparition-cli.exe" firewall remove "$INSTDIR\Apparition.exe" "$INSTDIR\apparition-cli.exe"'
  Pop $0
  ${If} $Mode == "system"
    nsExec::Exec '"$INSTDIR\apparition-cli.exe" link --system --remove'
  ${Else}
    nsExec::Exec '"$INSTDIR\apparition-cli.exe" link --remove'
  ${EndIf}
  Pop $0

  ; "Open at login", when it starts this copy.
  ReadRegStr $0 HKCU "${AUTOSTART}" "Apparition"
  ${UnStrStr} $1 "$0" "$INSTDIR\"
  ${If} $1 != ""
    DeleteRegValue HKCU "${AUTOSTART}" "Apparition"
  ${EndIf}

  Delete "$SMPROGRAMS\Apparition.lnk"
  Delete "$INSTDIR\Apparition.exe"
  Delete "$INSTDIR\apparition-cli.exe"
  Delete "$INSTDIR\WebView2Loader.dll"
  Delete "$INSTDIR\uninstall.exe"
  RMDir "$INSTDIR"
  DeleteRegKey SHCTX "${UNINSTALL}"
  ; Pairings, signing and settings stay where they are (%APPDATA%\Apparition Installer).
SectionEnd
