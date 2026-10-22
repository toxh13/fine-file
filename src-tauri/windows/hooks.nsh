; fine-file installer hooks (tauri.conf.json: bundle > windows > nsis > installerHooks).
;
; Windows caches a program's icon by the program file's path. Installing a new
; version over an old one keeps that path, so the taskbar, the Start menu and
; the shortcuts can go on showing the old version's icon. After the files and
; shortcuts are in place, the shell is told to read the icons again.

!include LogicLib.nsh

!macro NSIS_HOOK_POSTINSTALL
  ; Rebuilds the icon cache of the account that runs the installer: the
  ; 64-bit tool where there is one (the installer is a 32-bit program, which
  ; reaches System32 only through Sysnative). Not waited for; a cache it
  ; cannot rebuild costs nothing but the old icon.
  ${If} ${FileExists} "$WINDIR\Sysnative\ie4uinit.exe"
    Exec '"$WINDIR\Sysnative\ie4uinit.exe" -show'
  ${Else}
    Exec '"$SYSDIR\ie4uinit.exe" -show'
  ${EndIf}
  ; SHCNE_ASSOCCHANGED with SHCNF_FLUSH: the shell draws its icons anew.
  System::Call 'shell32::SHChangeNotify(i 0x08000000, i 0x1000, p 0, p 0)'
!macroend
