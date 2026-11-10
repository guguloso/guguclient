; Lunar uninstaller additions:
;  - Restores hijacked Roblox protocol handlers
;  - Removes the startup login item
;  - Sends uninstall analytics
;  - Opens the uninstaller page

!include "FileFunc.nsh"
!include "TextFunc.nsh"

; Registry state written by the app, keep in sync with protocol and analytics constants
!define LUNAR_KEY "Software\LunarForRoblox"
!define PROTOCOL_BACKUP_KEY "${LUNAR_KEY}\ProtocolHandlerBackup"
!define ANALYTICS_OPT_OUT_VALUE "AnalyticsOptOut"
!define LUNAR_DATA_DIR "$PROFILE\.lunar-for-roblox"
!define ANALYTICS_ENDPOINT "https://analytics.lunarrobloxprod.com/ingest/event/batch"
!define NIL_UUID "00000000-0000-0000-0000-000000000000"
!define LOGIN_ITEM_RUN_VALUE "com.moonsworth.roblox"
!define UNINSTALL_PAGE_URL "https://www.lunar.gg/roblox/uninstaller"

; Target arch from electron-builder's per-arch package defines
!ifdef APP_ARM64
  !define ANALYTICS_CPU_ARCH "arm64"
!else ifdef APP_64
  !define ANALYTICS_CPU_ARCH "x64"
!else
  !define ANALYTICS_CPU_ARCH "ia32"
!endif

; Restores a Roblox protocol handler if it was hijacked by Lunar. Without a
; backup, deletes only the launcher-written values (see removeSchemeRegistration).
!macro restoreRobloxProtocolHandler SCHEME
  ReadRegStr $0 HKCU "Software\Classes\${SCHEME}\shell\open\command" ""
  ${If} $0 == '"$INSTDIR\${APP_EXECUTABLE_FILENAME}" "%1"'
    ReadRegStr $1 HKCU "${PROTOCOL_BACKUP_KEY}" "${SCHEME}"
    ${If} $1 != ""
      ReadRegStr $2 HKCU "${PROTOCOL_BACKUP_KEY}" "${SCHEME}.type"
      ${If} $2 == "REG_EXPAND_SZ"
        WriteRegExpandStr HKCU "Software\Classes\${SCHEME}\shell\open\command" "" $1
      ${Else}
        WriteRegStr HKCU "Software\Classes\${SCHEME}\shell\open\command" "" $1
      ${EndIf}
    ${Else}
      DeleteRegValue HKCU "Software\Classes\${SCHEME}\shell\open\command" ""
      DeleteRegValue HKCU "Software\Classes\${SCHEME}" "URL Protocol"
      DeleteRegValue HKCU "Software\Classes\${SCHEME}" ""
    ${EndIf}
  ${EndIf}
!macroend

; Opens the uninstall page and posts launcher.uninstall. An analytics
; opt-out skips the analytics POST and leaves out the installation ID.
!macro openUninstallPageAndSendAnalytics
  ReadRegStr $0 HKCU "${LUNAR_KEY}" "${ANALYTICS_OPT_OUT_VALUE}"
  ${If} $0 == "1"
    ExecShell "open" "${UNINSTALL_PAGE_URL}"
  ${Else}
    StrCpy $2 "${NIL_UUID}"
    FileOpen $0 "${LUNAR_DATA_DIR}\installation-id" r
    ${If} $0 != ""
      FileRead $0 $1
      FileClose $0
      ${un.TrimNewLines} $1 $1
      ${If} $1 != ""
        StrCpy $2 $1
      ${EndIf}
    ${EndIf}

    ; Open first so the page appears while the analytics POST runs
    ExecShell "open" "${UNINSTALL_PAGE_URL}?installId=$2"

    ; Windows version in node os.release() form (major.minor.build)
    ReadRegDWORD $3 HKLM "SOFTWARE\Microsoft\Windows NT\CurrentVersion" "CurrentMajorVersionNumber"
    ReadRegDWORD $4 HKLM "SOFTWARE\Microsoft\Windows NT\CurrentVersion" "CurrentMinorVersionNumber"
    ReadRegStr $5 HKLM "SOFTWARE\Microsoft\Windows NT\CurrentVersion" "CurrentBuildNumber"
    ${If} $3 == ""
      StrCpy $3 "unknown"
    ${Else}
      StrCpy $3 "$3.$4.$5"
    ${EndIf}
    ; "LS" is GetSystemTime, i.e. UTC, matching the Z-suffixed timestamp
    ${un.GetTime} "" "LS" $R0 $R1 $R2 $R3 $R4 $R5 $R6
    inetc::post '{"events":[{"type":"launcher.uninstall","data":{"event_id":"${NIL_UUID}","timestamp":"$R2-$R1-$R0T$R4:$R5:$R6.000Z","installation_id":"$2","session_id":"${NIL_UUID}","roblox_user_id":null,"operating_system":"win32","operating_system_release":"$3","cpu_architecture":"${ANALYTICS_CPU_ARCH}","app_version":"${VERSION}","online":true}}]}' /HEADER "Content-Type: application/json" /SILENT /TOSTACK "${ANALYTICS_ENDPOINT}" /END
    Pop $0
    Delete "${LUNAR_DATA_DIR}\installation-id"
  ${EndIf}
!macroend

!macro customUnInstall
  ; Installs over an existing copy can run the previous uninstaller with
  ; --updated (installSection.nsh). Restrict all of this to real uninstalls.
  ${ifNot} ${isUpdated}
    !insertmacro restoreRobloxProtocolHandler "roblox-player"
    !insertmacro restoreRobloxProtocolHandler "roblox"
    !insertmacro restoreRobloxProtocolHandler "roblox-studio"

    !insertmacro openUninstallPageAndSendAnalytics

    ; Open-on-startup login item and its Task Manager enable/disable state.
    DeleteRegValue HKCU "Software\Microsoft\Windows\CurrentVersion\Run" "${LOGIN_ITEM_RUN_VALUE}"
    DeleteRegValue HKCU "Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run" "${LOGIN_ITEM_RUN_VALUE}"

    ; All app state under Software\LunarForRoblox is ours, safe to fully delete it
    DeleteRegKey HKCU "${LUNAR_KEY}"
  ${endIf}
!macroend
