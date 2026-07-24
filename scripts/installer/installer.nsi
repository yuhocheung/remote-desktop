; RdCore Remote Desktop Host — Windows 安装包 (NSIS)
;
; 注意：本文件须以「UTF-8 + BOM」保存，否则 NSIS 会把中文按 ANSI 解析而编译失败。
;       （makensis 在检测到 BOM 时才会按 UTF-8 读取。）
;
; 打包内容（来自构建期 DIST 目录）：
;   rdcore-desktop.exe   —— 受控端主程序（带 service feature，可注册为 Windows 服务）
;   rdcore-banner.exe    —— OS 不可伪造横幅子进程（Host 运行时从同目录拉起）
;   LICENSE              —— 许可文件（控制面板“卸载”里展示）
;
; 行为：
;   * 需要管理员权限（安装到 Program Files、写 HKLM 卸载项）
;   * 安装到 $PROGRAMFILES64\RdCore
;   * 用 setx /M 注入 RDCORE_SIGNALING（受控端运行时读取）
;   * 在当前登录用户会话中启动 Host（托盘图标 + 配对二维码对用户可见）；
;     不用 Windows 服务承载——服务跑在 Session 0，用户看不到任何界面
;   * 可选“登录后自动启动”（HKLM Run 键，对全体用户生效）
;   * 卸载：杀残留进程 → 清 Run 键/快捷方式 → 清环境 → 删文件 → 清注册表

!include "MUI2.nsh"
!include "nsDialogs.nsh"
!include "x64.nsh"

; ---------------------------------------------------------------------------
; 配置
; ---------------------------------------------------------------------------
!ifndef DIST
  !define DIST "dist"
!endif

!define APPNAME      "RdCore Remote Desktop Host"
!define APPVENDOR    "RdCore"
!define EXE          "rdcore-desktop.exe"
!define BANNER_EXE   "rdcore-banner.exe"
!define ICO          "icon.ico"
; 源图标：仓库根目录的 icon.ico（单一事实来源，由用户在项目根放置）。
!define ICO_SRC      "..\..\icon.ico"
!define RUNKEY       "Software\Microsoft\Windows\CurrentVersion\Run"
!define UNINST_KEY   "Software\Microsoft\Windows\CurrentVersion\Uninstall\${APPNAME}"

Name    "${APPNAME}"
OutFile "RdCore-Host-Setup.exe"
InstallDir "$PROGRAMFILES64\${APPVENDOR}"

RequestExecutionLevel admin

VIProductVersion "0.1.0.0"
VIAddVersionKey "ProductName"    "${APPNAME}"
VIAddVersionKey "CompanyName"    "${APPVENDOR}"
VIAddVersionKey "FileVersion"    "0.1.0.0"
VIAddVersionKey "InternalName"   "${EXE}"
VIAddVersionKey "OriginalFilename" "RdCore-Host-Setup.exe"
VIAddVersionKey "FileDescription" "RdCore Remote Desktop Host Installer"
VIAddVersionKey "LegalCopyright"  "(c) RdCore. AGPL-3.0."

; ---------------------------------------------------------------------------
; 页面
; ---------------------------------------------------------------------------
!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_LICENSE "${DIST}\LICENSE"
!insertmacro MUI_PAGE_DIRECTORY
Page custom SignalPageCreate SignalPageLeave
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH

!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES

!insertmacro MUI_LANGUAGE "SimpChinese"

; ---------------------------------------------------------------------------
; 变量（信令配置页）
; ---------------------------------------------------------------------------
Var Dialog
Var SignalingEdit
Var SignalingLabel
Var AutostartCheckBox
Var SignalingUrl
Var AutostartState

; ---------------------------------------------------------------------------
; 信令配置自定义页
; ---------------------------------------------------------------------------
Function SignalPageCreate
  !insertmacro MUI_HEADER_TEXT "信令服务器配置" "设置受控端连接的信令服务器地址"

  nsDialogs::Create 1018
  Pop $Dialog
  ${If} $Dialog == error
    Abort
  ${EndIf}

  ; 首次进入时给一个默认值，避免空框
  ${If} $SignalingUrl == ""
    StrCpy $SignalingUrl "ws://8.138.237.243:8080"
  ${EndIf}
  ${If} $AutostartState == ""
    StrCpy $AutostartState "1"
  ${EndIf}

  ${NSD_CreateLabel} 0 0 100% 24u \
    "信令服务器地址 (WebSocket)，受控端会连接它等待控制端。例如 ws://8.138.237.243:8080 或 wss://rdcore.example.com"
  Pop $SignalingLabel

  ${NSD_CreateText} 0 30u 100% 12u "$SignalingUrl"
  Pop $SignalingEdit

  ${NSD_CreateCheckBox} 0 48u 100% 12u "登录 Windows 后自动启动受控端（推荐）"
  Pop $AutostartCheckBox
  ${If} $AutostartState == "1"
    ${NSD_Check} $AutostartCheckBox
  ${EndIf}

  nsDialogs::Show
FunctionEnd

Function SignalPageLeave
  ${NSD_GetText} $SignalingEdit $SignalingUrl
  ${NSD_GetState} $AutostartCheckBox $AutostartState
  ${If} $SignalingUrl == ""
    MessageBox MB_OK|MB_ICONEXCLAMATION "请填写信令服务器地址。"
    Abort
  ${EndIf}
FunctionEnd

; ---------------------------------------------------------------------------
; 安装段
; ---------------------------------------------------------------------------
Section "Install" SEC_INSTALL
  SetShellVarContext all
  SetRegView 64   ; 写入原生 64 位注册表视图（避免落到 Wow6432Node）
  SetOutPath $INSTDIR

  File "${DIST}\${EXE}"
  File "${DIST}\${BANNER_EXE}"
  ; FFmpeg 运行库（av*-61 系，hwcodec 硬编依赖；构建脚本从 FFMPEG_DIR/bin 拷入 DIST）
  File "${DIST}\*.dll"
  File "${DIST}\LICENSE"
  File "${ICO_SRC}"

  ; 卸载程序
  WriteUninstaller "$INSTDIR\uninstall.exe"

  ; 控制面板卸载项
  WriteRegStr HKLM "${UNINST_KEY}" "DisplayName"     "${APPNAME}"
  WriteRegStr HKLM "${UNINST_KEY}" "UninstallString" "$\"$INSTDIR\uninstall.exe$\""
  WriteRegStr HKLM "${UNINST_KEY}" "DisplayIcon"     "$INSTDIR\${ICO}"
  WriteRegStr HKLM "${UNINST_KEY}" "InstallLocation" "$INSTDIR"
  WriteRegStr HKLM "${UNINST_KEY}" "Publisher"       "${APPVENDOR}"
  WriteRegStr HKLM "${UNINST_KEY}" "DisplayVersion"  "0.1.0"

  ; 注入信令地址（受控端运行时读取）
  ExecWait 'setx /M RDCORE_SIGNALING "$SignalingUrl"'

  ; 开始菜单快捷方式（手动启动入口；--signal 显式传入，免疫安装前残留的 stale 环境变量）
  CreateDirectory "$SMPROGRAMS\${APPNAME}"
  CreateShortcut  "$SMPROGRAMS\${APPNAME}\${APPNAME}.lnk" "$\"$INSTDIR\${EXE}$\"" "run --signal $\"$SignalingUrl$\"" "$INSTDIR\${ICO}"

  ; 登录后自动启动（HKLM Run 键：对全体用户生效，不受 UAC 提权账户差异影响）。
  ; 注意：不用 Windows 服务承载——服务跑在 Session 0，用户看不到托盘图标与配对二维码。
  ${If} $AutostartState == "1"
    WriteRegStr HKLM "${RUNKEY}" "RdCoreHost" "$\"$INSTDIR\${EXE}$\" run --signal $\"$SignalingUrl$\""
  ${EndIf}

  ; 立即在当前用户会话启动（托盘图标可见，点开可查配对二维码）
  Exec '"$INSTDIR\${EXE}" run --signal "$SignalingUrl"'

  SectionEnd

; ---------------------------------------------------------------------------
; 卸载段
; ---------------------------------------------------------------------------
Section "Uninstall"
  SetShellVarContext all
  SetRegView 64   ; 与原安装一致的 64 位注册表视图
  ; 清登录自启项（与安装段一致写 HKLM；同时清掉旧版可能写的 HKCU 项）
  DeleteRegValue HKLM "${RUNKEY}" "RdCoreHost"
  DeleteRegValue HKCU "${RUNKEY}" "RdCoreHost"

  ; 杀残留进程，避免文件被占用
  ExecWait 'taskkill /F /IM ${BANNER_EXE} /T'
  ExecWait 'taskkill /F /IM ${EXE} /T'

  ; 清环境变量
  ExecWait 'reg delete "HKLM\SYSTEM\CurrentControlSet\Control\Session Manager\Environment" /V RDCORE_SIGNALING /F'

  ; 删文件与快捷方式
  Delete "$INSTDIR\${EXE}"
  Delete "$INSTDIR\${BANNER_EXE}"
  Delete "$INSTDIR\*.dll"
  Delete "$INSTDIR\LICENSE"
  Delete "$INSTDIR\${ICO}"
  Delete "$INSTDIR\uninstall.exe"
  RMDir  "$INSTDIR"
  Delete "$SMPROGRAMS\${APPNAME}\${APPNAME}.lnk"
  RMDir  "$SMPROGRAMS\${APPNAME}"

  ; 清注册表
  DeleteRegKey HKLM "${UNINST_KEY}"
SectionEnd
