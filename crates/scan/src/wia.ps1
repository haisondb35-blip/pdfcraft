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
    if ($null -ne $p) { try { $p.Value = $value } catch { } }
}

# Save a scanned page as $dst: JPEG (quality 85) for colour and grey, PNG for black and white,
# turned grey when the scanner could only scan in colour. $src is removed unless it is $dst.
function SavePage($src, $dst, $mode) {
    Add-Type -AssemblyName System.Drawing
    $img = [System.Drawing.Image]::FromFile($src)
    try {
        $page = $img
        if ($mode -eq 'gray' -or $mode -eq 'bw') {
            $page = New-Object System.Drawing.Bitmap $img.Width, $img.Height
            $page.SetResolution($img.HorizontalResolution, $img.VerticalResolution)
            $g = [System.Drawing.Graphics]::FromImage($page)
            $m = New-Object System.Drawing.Imaging.ColorMatrix
            foreach ($r in 0, 1, 2) { $m[0, $r] = 0.299; $m[1, $r] = 0.587; $m[2, $r] = 0.114 }
            $a = New-Object System.Drawing.Imaging.ImageAttributes
            $a.SetColorMatrix($m)
            $g.DrawImage($img, (New-Object System.Drawing.Rectangle 0, 0, $img.Width, $img.Height), 0, 0, $img.Width, $img.Height, [System.Drawing.GraphicsUnit]::Pixel, $a)
            $g.Dispose()
        }
        if ($dst.EndsWith('.png')) {
            $page.Save($dst, [System.Drawing.Imaging.ImageFormat]::Png)
        } else {
            $codec = [System.Drawing.Imaging.ImageCodecInfo]::GetImageEncoders() | Where-Object { $_.MimeType -eq 'image/jpeg' } | Select-Object -First 1
            $params = New-Object System.Drawing.Imaging.EncoderParameters 1
            $params.Param[0] = New-Object System.Drawing.Imaging.EncoderParameter ([System.Drawing.Imaging.Encoder]::Quality, [long]85)
            $page.Save($dst, $codec, $params)
        }
        if ($page -ne $img) { $page.Dispose() }
    } finally {
        $img.Dispose()
    }
    if ($src -ne $dst) { Remove-Item -LiteralPath $src -Force -ErrorAction SilentlyContinue }
}

# Scan the whole feeder as one job through Windows.Devices.Scanners, the API of Windows' own Scan
# app. A network (WSD) scanner such as Brother's feeds the next sheet while it scans one, and
# says when the feeder is empty, only within one job: asking for one page a transfer (the WIA
# Automation Layer's way) ejected the sheet already fed, and the last transfer waited for paper.
# Returns $false, having done nothing, when the API can't reach the scanner (the caller then
# falls back); $true once it has scanned or said why it couldn't.
function FeederScan($info) {
    try {
        Add-Type -AssemblyName System.Runtime.WindowsRuntime
        $null = [Windows.Devices.Scanners.ImageScanner, Windows.Devices.Scanners, ContentType = WindowsRuntime]
        $null = [Windows.Devices.Enumeration.DeviceInformation, Windows.Devices.Enumeration, ContentType = WindowsRuntime]
        $null = [Windows.Storage.StorageFolder, Windows.Storage, ContentType = WindowsRuntime]
    } catch { return $false }
    $methods = [System.WindowsRuntimeSystemExtensions].GetMethods() | Where-Object { $_.Name -eq 'AsTask' -and $_.GetParameters().Count -eq 1 }
    $one = $methods | Where-Object { $_.GetParameters()[0].ParameterType.Name -eq 'IAsyncOperation`1' } | Select-Object -First 1
    $two = $methods | Where-Object { $_.GetParameters()[0].ParameterType.Name -eq 'IAsyncOperationWithProgress`2' } | Select-Object -First 1
    if ($null -eq $one -or $null -eq $two) { return $false }
    $await = {
        param($task)
        try { $task.Wait(-1) | Out-Null } catch { }
        if ($task.Exception) { throw $task.Exception.InnerException }
        $task.Result
    }
    $name = ''
    try { $name = "$($info.Properties.Item('Name').Value)" } catch { }
    try {
        $all = & $await ($one.MakeGenericMethod([Windows.Devices.Enumeration.DeviceInformationCollection]).Invoke($null, @([Windows.Devices.Enumeration.DeviceInformation]::FindAllAsync([Windows.Devices.Scanners.ImageScanner]::GetDeviceSelector()))))
    } catch { return $false }
    $match = @($all | Where-Object { $name -and $_.Name.StartsWith($name) })
    if ($match.Count -ne 1) { $match = if ($all.Count -eq 1) { @($all[0]) } else { @() } }
    if ($match.Count -ne 1) { return $false }
    # Opening a scanner that is still busy (waking, finishing a job) is retried for about 30 s.
    $scanner = $null
    for ($try = 0; $null -eq $scanner; $try++) {
        try {
            $scanner = & $await ($one.MakeGenericMethod([Windows.Devices.Scanners.ImageScanner]).Invoke($null, @([Windows.Devices.Scanners.ImageScanner]::FromIdAsync($match[0].Id))))
        } catch {
            $hex = '{0:X8}' -f $_.Exception.HResult
            if ($hex -eq '80210006' -and $try -lt 10) { Start-Sleep -Seconds 3; continue }
            if ($hex -eq '80210006') { Fail @{ Exception = $_.Exception }; return $true }
            return $false
        }
    }
    if ($null -eq $scanner -or -not $scanner.IsScanSourceSupported('Feeder')) { return $false }
    $mode = "$env:PDFCRAFT_SCAN_MODE"
    $dpi = [int]$env:PDFCRAFT_SCAN_DPI
    $max = [int]$env:PDFCRAFT_SCAN_MAX_PAGES
    $out = $env:PDFCRAFT_SCAN_OUT
    $cfg = $scanner.FeederConfiguration
    # Every sheet (0) unless a limit was asked for.
    $cfg.MaxNumberOfPages = [uint32][math]::Max(0, $max)
    $cfg.Duplex = ($env:PDFCRAFT_SCAN_SOURCE -eq 'duplex') -and $cfg.CanScanDuplex
    $want = switch ($mode) { 'gray' { 'Grayscale' } 'bw' { 'Monochrome' } default { 'Color' } }
    $cfg.ColorMode = if ($cfg.IsColorModeSupported($want)) { $want } elseif ($cfg.IsColorModeSupported('Grayscale') -and $want -ne 'Color') { 'Grayscale' } else { 'Color' }
    $d = [math]::Min([math]::Max($dpi, [int]$cfg.MinResolution.DpiX), [int]$cfg.MaxResolution.DpiX)
    $res = New-Object Windows.Devices.Scanners.ImageScannerResolution
    $res.DpiX = $d; $res.DpiY = $d
    $cfg.DesiredResolution = $res
    $cfg.Format = if ($mode -ne 'bw' -and $cfg.IsFormatSupported('Jpeg')) { 'Jpeg' } elseif ($cfg.IsFormatSupported('Png')) { 'Png' } else { 'DeviceIndependentBitmap' }
    try {
        $folder = & $await ($one.MakeGenericMethod([Windows.Storage.StorageFolder]).Invoke($null, @([Windows.Storage.StorageFolder]::GetFolderFromPathAsync($out))))
        $result = & $await ($two.MakeGenericMethod([Windows.Devices.Scanners.ImageScannerScanResult], [uint32]).Invoke($null, @($scanner.ScanFilesToFolderAsync('Feeder', $folder))))
    } catch {
        Fail @{ Exception = $_.Exception }
        return $true
    }
    $ext = if ($mode -eq 'bw') { 'png' } else { 'jpg' }
    $n = 0
    foreach ($f in $result.ScannedFiles) {
        $n++
        $file = Join-Path $out ('page-{0:D4}.{1}' -f $n, $ext)
        $grey = $cfg.ColorMode -eq 'Color' -and $mode -ne 'color' -and $mode -ne ''
        if ($f.Path.EndsWith(".$ext") -and -not $grey) { Move-Item -LiteralPath $f.Path -Destination $file -Force } else { SavePage $f.Path $file $mode }
        Emit @{ page = $n; file = $file; dpi = $d }
    }
    # An empty feeder at the start is "no paper", as the WIA path says it.
    if ($n -eq 0) { Emit @{ error = 'empty'; code = '80210003'; message = 'the feeder is empty' } }
    return $true
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
            # Windows' own WSD driver reports itself as the maker; the name already says the brand.
            if ($maker -eq 'Microsoft') { $maker = '' }
            $sources = @('flatbed')
            try {
                # The paper sources are a property of the connected device (a WSD network scanner
                # lists none before it is connected); an offline one keeps the device info's, if any.
                $caps = $null
                try { $caps = Prop $info.Connect().Properties $DOC_HANDLING_CAPS } catch { }
                if ($null -eq $caps) { $caps = Prop $info.Properties $DOC_HANDLING_CAPS }
                if ($null -ne $caps) {
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
    if ($null -eq $info) { Emit @{ error = 'offline'; code = '80210005'; message = 'the scanner is not connected' }; exit 0 }
    # The feeder goes through Windows.Devices.Scanners first (see FeederScan); the loop below is
    # the fallback when that API can't reach the scanner.
    if (($env:PDFCRAFT_SCAN_SOURCE -eq 'feeder' -or $env:PDFCRAFT_SCAN_SOURCE -eq 'duplex') -and (FeederScan $info)) { exit 0 }
    $dev = $info.Connect()
    $source = $env:PDFCRAFT_SCAN_SOURCE
    $feed = $source -eq 'feeder' -or $source -eq 'duplex'
    $select = switch ($source) { 'feeder' { $FEEDER } 'duplex' { $FEEDER -bor $DUPLEX } default { $FLATBED } }
    SetProp $dev.Properties $DOC_HANDLING_SELECT $select
    # One page a transfer: the loop below asks again until the feeder is empty. (Pages = 0, "every
    # sheet", is refused as an invalid parameter by network scanners.)
    if ($feed) { SetProp $dev.Properties $PAGES 1 }
    $item = $dev.Items.Item(1)
    $dpi = [int]$env:PDFCRAFT_SCAN_DPI
    $intent = switch ($env:PDFCRAFT_SCAN_MODE) { 'gray' { 2 } 'bw' { 4 } default { 1 } }
    SetProp $item.Properties $INTENT $intent
    SetProp $item.Properties $XRES $dpi
    SetProp $item.Properties $YRES $dpi
    $format = if ($env:PDFCRAFT_SCAN_MODE -eq 'bw') { $FORMAT_PNG } else { $FORMAT_JPEG }
    $ext = if ($format -eq $FORMAT_PNG) { 'png' } else { 'jpg' }
    # Ask only for a format the scanner offers: network (WSD) scanners often give BMP alone, and
    # asking for another made the transfer come back in a form that couldn't be used.
    $offered = @(); try { foreach ($f in $item.Formats) { $offered += "$f" } } catch { }
    $transfer = if ($offered -contains $format) { $format } elseif ($offered -contains $FORMAT_PNG) { $FORMAT_PNG } else { $FORMAT_BMP }
    $out = $env:PDFCRAFT_SCAN_OUT
    $max = [int]$env:PDFCRAFT_SCAN_MAX_PAGES
    $n = 0
    $last = $null
    while ($true) {
        $img = $null
        # Busy: before the first page the scanner may be waking or finishing another job; between
        # feeder pages a network (WSD) scanner is busy while it feeds and scans the next sheet of
        # the same job. Wait for it either way, but only so long (about 30 s before the first page,
        # 20 s between pages): once the wait is up the batch ends with the pages so far, so a
        # scanner that stays busy can't keep the job running. (Ending at the first busy left the
        # rest of the sheets scanned but never read, and the next Scan got a stale page.)
        $waits = 10
        $pause = if ($n -eq 0) { 3 } else { 2 }
        for ($try = 0; $null -eq $img; $try++) {
            try {
                $img = $item.Transfer($transfer)
            } catch {
                $hex = '{0:X8}' -f $_.Exception.HResult
                if ($hex -eq '80210006' -and $try -lt $waits) { Start-Sleep -Seconds $pause; continue }
                # The feeder ran out of paper, or stayed busy, after at least one page: the batch is done.
                if ($feed -and $n -gt 0 -and ($hex -eq '80210003' -or $hex -eq '80210006')) { break }
                Fail $_
                exit 0
            }
        }
        if ($null -eq $img) { break }
        # A page identical to the one before is the driver handing the same image back, not a new
        # sheet: drop it and end the batch rather than add a copy.
        $bytes = $null
        try { $bytes = [byte[]]$img.FileData.BinaryData } catch { }
        if ($null -ne $bytes) {
            $sum = [BitConverter]::ToString([Security.Cryptography.SHA256]::Create().ComputeHash($bytes))
            if ($sum -eq $last) { break }
            $last = $sum
        }
        $n++
        $file = Join-Path $out ('page-{0:D4}.{1}' -f $n, $ext)
        if ("$($img.FormatID)" -eq $format) {
            $img.SaveFile($file)
        } else {
            # Another format came back (BMP, PNG): save it and re-encode it with Windows' own
            # imaging, JPEG for colour and grey (a 300 dpi page is ~25 MB as BMP), PNG for black and white.
            $raw = Join-Path $out ('page-{0:D4}.raw.{1}' -f $n, $img.FileExtension)
            $img.SaveFile($raw)
            Add-Type -AssemblyName System.Drawing
            $bmp = [System.Drawing.Image]::FromFile($raw)
            try {
                if ($ext -eq 'jpg') {
                    $codec = [System.Drawing.Imaging.ImageCodecInfo]::GetImageEncoders() | Where-Object { $_.MimeType -eq 'image/jpeg' } | Select-Object -First 1
                    $params = New-Object System.Drawing.Imaging.EncoderParameters 1
                    $params.Param[0] = New-Object System.Drawing.Imaging.EncoderParameter ([System.Drawing.Imaging.Encoder]::Quality, [long]85)
                    $bmp.Save($file, $codec, $params)
                } else {
                    $bmp.Save($file, [System.Drawing.Imaging.ImageFormat]::Png)
                }
            } finally {
                $bmp.Dispose()
                Remove-Item -LiteralPath $raw -Force -ErrorAction SilentlyContinue
            }
        }
        Emit @{ page = $n; file = $file; dpi = $dpi }
        if (-not $feed -or ($max -gt 0 -and $n -ge $max)) { break }
    }
    exit 0
} catch {
    Fail $_
    exit 0
}
