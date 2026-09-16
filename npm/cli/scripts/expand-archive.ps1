# Unpack a .zip for scripts/postinstall.js on Windows, where `unzip` is not
# present on a stock image.
#
# Invoked with `powershell.exe -File`, NOT `-Command`. This matters: `-Command`
# does not bind trailing arguments to `$args` — it APPENDS them to the command
# text and evaluates the result. An earlier version of this code passed the
# archive and destination as trailing arguments to a `-Command` string that
# read `$args[0]` / `$args[1]`; `$args.Count` is 0 under `-Command`, so
# Expand-Archive ran with null paths and the Windows fallback silently produced
# no binary. That append is also an injection shape — text from the caller
# becomes part of the script — which `-File` avoids entirely, because
# everything after the script path is bound as a parameter VALUE and is never
# parsed as code.
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$LiteralPath,
    [Parameter(Mandatory = $true)][string]$DestinationPath
)

$ErrorActionPreference = 'Stop'

try {
    Expand-Archive -LiteralPath $LiteralPath -DestinationPath $DestinationPath -Force
    exit 0
} catch {
    [Console]::Error.WriteLine($_.Exception.Message)
    exit 1
}
