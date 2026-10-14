# PdfCraft scanning through Windows Image Acquisition (the WIA Automation Layer, wiaaut.dll,
# part of Windows). Run by pdfcraft-scan; settings come in environment variables, results go out
# as one JSON object per line:
#   {"device":{"id":..,"name":..,"maker":..,"sources":[..]}}   (op = list)
#   {"page":n,"file":path}                                      (op = scan, one per page)
#   {"error":"offline|busy|jam|empty|cover|cancelled|nowia|failed","code":hresult,"message":..}
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
[Console]::OutputEncoding = [Text.Encoding]::UTF8

function Emit($o) { [Console]::Out.WriteLine(($o | ConvertTo-Json -Compress -Depth 4)); [Console]::Out.Flush() }

function Fail($e) {
    $code = 0
    if ($e.Exception -and $e.Exception.HResult) { $code = $e.Exception.HResult }
    $hex = '{0:X8}' -f $code
    $kind = switch ($hex) {
        '80210003' { 'empty' }
        '80210002' { 'jam' }
        '80210005' { 'offline' }
        '80210006' { 'busy' }
        '80210009' { 'cover' }
        '8021000E' { 'offline' }
        '80210015' { 'offline' }
        '80210064' { 'cancelled' }
        '80004004' { 'cancelled' }
        default { 'failed' }
    }
    Emit @{ error = $kind; code = $hex; message = "$($e.Exception.Message)".Trim() }
}

# Property ids (wiadef.h, documented in the WIA reference).
$DOC_HANDLING_CAPS = 3086; $DOC_HANDLING_SELECT = 3088; $PAGES = 3096
$INTENT = 6146; $XRES = 6147; $YRES = 6148
$FEEDER = 1; $FLATBED = 2; $DUPLEX = 4
$FORMAT_PNG = '{B96B3CAF-0728-11D3-9D7B-0000F81EF32E}'
$FORMAT_JPEG = '{B96B3CAE-0728-11D3-9D7B-0000F81EF32E}'
$FORMAT_BMP = '{B96B3CAB-0728-11D3-9D7B-0000F81EF32E}'

function Prop($props, $id) {
    foreach ($p in $props) { if ($p.PropertyID -eq $id) { return $p } }
    return $null
}

function SetProp($props, $id, $value) {
    $p = Prop $props $id
    if ($p -ne $null) { try { $p.Value = $value } catch { } }
}

try {
    try { $dm = New-Object -ComObject WIA.DeviceManager } catch { Emit @{ error = 'nowia'; code = '0'; message = "$($_.Exception.Message)".Trim() }; exit 0 }
    $op = $env:PDFCRAFT_SCAN_OP
    if ($op -eq 'list') {
        foreach ($info in $dm.DeviceInfos) {
            if ($info.Type -ne 1) { continue }  # ScannerDeviceType
            $name = ''; $maker = ''
            try { $name = $info.Properties.Item('Name').Value } catch { }
            try { $maker = $info.Properties.Item('Manufacturer').Value } catch { }
            $sources = @('flatbed')
            try {
                $caps = Prop $info.Properties $DOC_HANDLING_CAPS
                if ($caps -ne $null) {
                    $sources = @()
                    if ($caps.Value -band $FLATBED) { $sources += 'flatbed' }
                    if ($caps.Value -band $FEEDER) { $sources += 'feeder' }
                    if ($caps.Value -band $DUPLEX) { $sources += 'duplex' }
                    if ($sources.Count -eq 0) { $sources = @('flatbed') }
                }
            } catch { }
            Emit @{ device = @{ id = $info.DeviceID; name = "$name"; maker = "$maker"; sources = $sources } }
        }
        exit 0
    }

    $info = $null
    foreach ($i in $dm.DeviceInfos) { if ($i.DeviceID -eq $env:PDFCRAFT_SCAN_DEVICE) { $info = $i } }
    if ($info -eq $null) { Emit @{ error = 'offline'; code = '80210005'; message = 'the scanner is not connected' }; exit 0 }
    $dev = $info.Connect()
    $source = $env:PDFCRAFT_SCAN_SOURCE
    $feed = $source -eq 'feeder' -or $source -eq 'duplex'
    $select = switch ($source) { 'feeder' { $FEEDER } 'duplex' { $FEEDER -bor $DUPLEX } default { $FLATBED } }
    SetProp $dev.Properties $DOC_HANDLING_SELECT $select
    if ($feed) { SetProp $dev.Properties $PAGES 0 }  # 0: every sheet in the feeder
    $item = $dev.Items.Item(1)
    $dpi = [int]$env:PDFCRAFT_SCAN_DPI
    $intent = switch ($env:PDFCRAFT_SCAN_MODE) { 'gray' { 2 } 'bw' { 4 } default { 1 } }
    SetProp $item.Properties $INTENT $intent
    SetProp $item.Properties $XRES $dpi
    SetProp $item.Properties $YRES $dpi
    $format = if ($env:PDFCRAFT_SCAN_MODE -eq 'bw') { $FORMAT_PNG } else { $FORMAT_JPEG }
    $ext = if ($format -eq $FORMAT_PNG) { 'png' } else { 'jpg' }
    $out = $env:PDFCRAFT_SCAN_OUT
    $max = [int]$env:PDFCRAFT_SCAN_MAX_PAGES
    $n = 0
    while ($true) {
        try {
            $img = $item.Transfer($format)
        } catch {
            $hex = '{0:X8}' -f $_.Exception.HResult
            # The feeder ran out of paper after at least one page: the batch is done.
            if ($feed -and $n -gt 0 -and $hex -eq '80210003') { break }
            Fail $_
            exit 0
        }
        # Some drivers ignore the format asked for: convert what came back.
        if ($img.FormatID -ne $format) {
            $ip = New-Object -ComObject WIA.ImageProcess
            $ip.Filters.Add($ip.FilterInfos.Item('Convert').FilterID)
            $ip.Filters.Item(1).Properties.Item('FormatID').Value = $format
            $img = $ip.Apply($img)
        }
        $n++
        $file = Join-Path $out ('page-{0:D4}.{1}' -f $n, $ext)
        $img.SaveFile($file)
        Emit @{ page = $n; file = $file; dpi = $dpi }
        if (-not $feed -or ($max -gt 0 -and $n -ge $max)) { break }
    }
    exit 0
} catch {
    Fail $_
    exit 0
}
