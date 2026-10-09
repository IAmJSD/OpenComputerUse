# Builds OcuPanelHook.dll with MSVC (run inside a Developer shell). The hook
# runs inside other apps, so it links the C runtime statically (/MT) and the
# build fails if it imports anything but Windows system DLLs.
param([Parameter(Mandatory)] [string] $Out)
$ErrorActionPreference = 'Stop'

New-Item -ItemType Directory -Force -Path (Split-Path $Out) | Out-Null
cl /nologo /W4 /WX /O2 /LD /std:c11 /DUNICODE /D_UNICODE /D_WIN32_WINNT=0x0601 /MT `
    "/Fe:$Out" "/Fo:$(Split-Path $Out)\" "$PSScriptRoot\hook.c" `
    /link ole32.lib shell32.lib shlwapi.lib uuid.lib comdlg32.lib
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

# Dependents are the indented lines; "Dump of file" is not.
$imports = dumpbin /nologo /dependents $Out |
    Select-String -Pattern '^\s+(\S+\.dll)\s*$' |
    ForEach-Object { $_.Matches[0].Groups[1].Value }
$allowed = @('kernel32.dll', 'ole32.dll', 'shell32.dll', 'shlwapi.dll', 'comdlg32.dll')
$stray = $imports | Where-Object { $allowed -notcontains $_.ToLower() }
if ($stray) {
    Write-Error "the hook imports $($stray -join ', '), which are not allowed"
    exit 1
}
Write-Host "hook imports: $($imports -join ', ')"
