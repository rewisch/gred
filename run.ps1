# Launch gred. On a box with no usable GPU (headless server, plain RDP session)
# eframe's OpenGL context fails or crashes, so we force Mesa's llvmpipe software
# renderer (`--software`) and make sure the bundled Mesa DLLs sit next to the exe.
#
#   .\run.ps1 [file]
param([string]$File)

$root = $PSScriptRoot
$exe  = Join-Path $root "target\release\gred.exe"
if (-not (Test-Path $exe)) { $exe = Join-Path $root "target\debug\gred.exe" }
if (-not (Test-Path $exe)) { Write-Error "build first: cargo build --release"; exit 1 }
$bin = Split-Path $exe

# Copy the software-GL DLLs beside the exe if we have them and they're missing.
foreach ($d in "opengl32.dll", "libgallium_wgl.dll", "dxil.dll") {
    $src = Join-Path $root "mesa\$d"
    $dst = Join-Path $bin  $d
    if ((Test-Path $src) -and -not (Test-Path $dst)) { Copy-Item $src $dst }
}

$gredArgs = @("--software")
if ($File) { $gredArgs += $File }
& $exe @gredArgs
