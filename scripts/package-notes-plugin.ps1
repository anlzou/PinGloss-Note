# package-notes-plugin.sh 的 Windows 形状（重构文档 §9 P1，钉在 x86_64-pc-windows-msvc）：
#   .artifacts/plugins/notes-<时间戳>/share/pingloss/plugins/notes/
#     {plugin.json,bin/{PinGloss-Note x Nomo.exe,vcruntime140.dll},LICENSE,licenses/,SOURCE.md}
#
# 这一格只**组装**、不自装不发布（AGENTS：scripts/ 的产物写 `.artifacts`，绝不自动安装／打标签／发布）。
# 清单 `entry.path` 仍写不带 `.exe` 的 `bin/PinGloss-Note x Nomo`：宿主 spawn.rs 2026-11-14 D-117 那一发是
# "先试精确路径、缺失再试平台后缀"，同一份清单两平台共用，不在这里分出 Windows 副本。
#
# ⚠ **本文件存成 UTF-8 带 BOM＋CRLF**，Windows PowerShell 5.1 与 PowerShell 7 都能跑。去掉 BOM 的那一刻
#    5.1 会把无 BOM 的 UTF-8 按 GBK 解码：中文注释与串里的字节对成双字节字，把收尾引号吞掉，parse 当场坏
#    （MissingEndParenthesisInMethodCall／字符串缺终止符那一形）。别把中文串改回"只留注释"——判据串与回显
#    本来就走中文，靠 BOM 钉编码才是解。
#
# 数据面按平台分形：Linux/Unix 用 socket，Windows 用宿主传入的 named pipe。`--probe` 只读 env、打进
# stdout 就退（main.rs 那一发排在最前），不碰显示与数据面；它能证身份，不能证真实握手。Windows 管道的
# 监听／连接／EOF／半帧由 `cargo test --bin nomo plugin::` 覆盖，真宿主唤起仍须单独验收。
#
# ⚠ 随 bin 带一颗 `vcruntime140.dll`：Rust MSVC 动态链 VC 运行库（口径同 package-windows.ps1 的
#    dumpbin 闸），没装 Build Tools 的机器才加载得动；Windows 的 DLL 搜索顺序先查 exe 自己那格目录，
#    所以 dll 与 `PinGloss-Note x Nomo.exe` 并排放在 `bin/` 里就够了。
#
# 清单那三位与 root Cargo.toml 解耦（重构文档 §5.4 第 2 条）：`version`＝插件包自己的号、
# `min_host`＝宿主能转发全部七条 op 的那一下限，都住在 `scripts/packaging/notes-plugin.json`，
# 由发布人主动改，不随 Nomo 涨版自动动。
[CmdletBinding()]
param([switch]$SkipBuild)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
# `$IsWindows` 是 PowerShell 7 才有的自动变量，5.1 在 StrictMode 下引用未定义变量当场炸；
# 平台这一位改走两侧都在的 `$env:OS`。
if ($env:OS -ne 'Windows_NT') { throw 'Run Windows packaging on Windows (Windows PowerShell 5.1 or PowerShell 7).' }

# 外部命令一律走 Start-Process 文件重定向，不用调用运算符 &。三个理由按本轮踩序记：
# ① nomo.exe 是 GUI subsystem（windows_subsystem）那颗——& 对这类进程不等待，5.1 里 $LASTEXITCODE 不置、
#    stdout 收空，`--probe` 那腿就这么哑掉的（cargo／dumpbin 是 console subsystem，前几闸才没露馅）；
# ② 5.1 在 EAP=Stop 下把 native stderr 判成终止错误，而 cargo 的人读输出本来就走 stderr；
# ③ .NET 双管道一先一后 ReadToEnd 有死锁形（cargo build 的 warning 超 4 KB 就卡）。
# 文件捕获这三样全免：进程完整退出才读文件，退码从 -PassThru 的 ExitCode 拿。交回 { Exit, Stdout, Stderr }。
function Invoke-Captured {
    param([string]$File, [string[]]$Arguments)
    $quoted = @($Arguments | ForEach-Object {
        if ($_ -match '\s') { '"' + $_ + '"' } else { $_ }
    })
    $outFile = [IO.Path]::GetTempFileName()
    $errFile = [IO.Path]::GetTempFileName()
    try {
        $proc = Start-Process -FilePath $File -ArgumentList $quoted -NoNewWindow -Wait -PassThru `
            -RedirectStandardOutput $outFile -RedirectStandardError $errFile
        $stdout = @([IO.File]::ReadAllText($outFile, [Text.Encoding]::UTF8) -split '\r?\n' | Where-Object { $_ -ne '' })
        $stderr = @([IO.File]::ReadAllText($errFile, [Text.Encoding]::UTF8) -split '\r?\n' | Where-Object { $_ -ne '' })
        [pscustomobject]@{ Exit = $proc.ExitCode; Stdout = $stdout; Stderr = $stderr }
    } finally {
        Remove-Item -LiteralPath $outFile, $errFile -Force -ErrorAction SilentlyContinue
    }
}

$root = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
Push-Location $root
try {
    # ① 环境闸：与 package-windows.ps1 同一条 x64 MSVC 工具链准备（VsDevShell 幂等，dev prompt 里照过）。
    $missingTools = @('cl.exe', 'link.exe', 'dumpbin.exe', 'rc.exe') | Where-Object {
        -not (Get-Command $_ -CommandType Application -ErrorAction SilentlyContinue)
    }
    if ($missingTools -or [string]::IsNullOrWhiteSpace($env:VCToolsRedistDir) -or $env:VSCMD_ARG_TGT_ARCH -ne 'x64') {
        $vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio/Installer/vswhere.exe'
        if (-not (Test-Path -LiteralPath $vswhere -PathType Leaf)) {
            throw 'Install Visual Studio C++ Build Tools and Windows SDK before packaging.'
        }
        $vs = Invoke-Captured -File $vswhere -Arguments @(
            '-latest', '-products', '*', '-requires', 'Microsoft.VisualStudio.Component.VC.Tools.x86.x64', '-property', 'installationPath')
        $vsPath = if ($vs.Exit -eq 0) { ($vs.Stdout | Select-Object -First 1) } else { '' }
        if ($vs.Exit -ne 0 -or [string]::IsNullOrWhiteSpace($vsPath)) {
            throw 'No Visual Studio installation with x64 C++ tools was found.'
        }
        $devShell = Join-Path $vsPath 'Common7/Tools/Launch-VsDevShell.ps1'
        if (-not (Test-Path -LiteralPath $devShell -PathType Leaf)) { throw "Missing $devShell; repair Visual Studio Build Tools." }
        & $devShell -Arch amd64 -HostArch amd64 -SkipAutomaticLocation
    }
    foreach ($tool in @('cargo.exe', 'cl.exe', 'link.exe', 'dumpbin.exe', 'rc.exe')) {
        if (-not (Get-Command $tool -CommandType Application -ErrorAction SilentlyContinue)) {
            throw "Missing $tool; install Rust MSVC, Visual Studio C++ Build Tools and Windows SDK."
        }
    }
    # GPUI 编译期要 fxc（着色器），口径同 package-windows.ps1；插件树本身不带它。
    if (-not (Get-Command fxc.exe -CommandType Application -ErrorAction SilentlyContinue) -and
        -not ($env:GPUI_FXC_PATH -and (Test-Path -LiteralPath $env:GPUI_FXC_PATH -PathType Leaf))) {
        throw 'Missing fxc.exe; install Windows SDK or set GPUI_FXC_PATH.'
    }
    if ([string]::IsNullOrWhiteSpace($env:VCToolsRedistDir)) { throw 'Visual Studio did not provide VCToolsRedistDir; repair the C++ tools installation.' }

    # ② 构建：与 .sh 同一发 cargo，钉 x86_64-pc-windows-msvc。
    $metadata = Invoke-Captured -File 'cargo' -Arguments @('metadata', '--locked', '--no-deps', '--format-version', '1')
    if ($metadata.Exit -ne 0) { throw 'cargo metadata failed' }
    $target = 'x86_64-pc-windows-msvc'
    if (-not $SkipBuild) {
        Write-Output "[构建] cargo build --locked --release --target $target --bin nomo（首编可达十几分钟，进度落进程结束后回显）"
        $build = Invoke-Captured -File 'cargo' -Arguments @('build', '--locked', '--release', '--target', $target, '--bin', 'nomo')
        # cargo 的人读进度走 stderr，成功也要让终端看见这一路（闸只看退码）。
        foreach ($line in @($build.Stdout) + @($build.Stderr)) { Write-Output $line }
        if ($build.Exit -ne 0) { throw 'Cargo release build failed' }
    }
    # Nomo 编出来的那颗就叫 `nomo.exe`；进插件树按清单 entry 补平台后缀摆成 `bin/PinGloss-Note x Nomo.exe`（内容同一颗）。
    $targetDirectory = (($metadata.Stdout -join "`n") | ConvertFrom-Json).target_directory
    $binary = Join-Path $targetDirectory "$target/release/nomo.exe"
    if (-not (Test-Path -LiteralPath $binary -PathType Leaf)) { throw "找不到产物 $binary（先不带 -SkipBuild 编一发）" }

    # ③ 产物闸：.sh 那头是 file(ELF)+ldd，这头对等成 PE 头＋dumpbin。
    $reader = New-Object IO.BinaryReader ([IO.File]::OpenRead($binary))
    try {
        if ($reader.ReadUInt16() -ne 0x5a4d) { throw 'Not a Windows PE executable' }
        $reader.BaseStream.Position = 0x3c
        $peOffset = $reader.ReadUInt32()
        $reader.BaseStream.Position = $peOffset
        if ($reader.ReadUInt32() -ne 0x00004550 -or $reader.ReadUInt16() -ne 0x8664) { throw 'Expected an x64 PE executable' }
    } finally { $reader.Dispose() }
    $deps = Invoke-Captured -File 'dumpbin' -Arguments @('/nologo', '/dependents', $binary)
    if ($deps.Exit -ne 0) { throw 'dumpbin dependency inspection failed.' }
    $importedDlls = @($deps.Stdout | ForEach-Object {
        if ($_ -match '^\s*([\w.-]+\.dll)\s*$') { $Matches[1].ToLowerInvariant() }
    } | Sort-Object -Unique)
    $runtimeImports = @($importedDlls | Where-Object { $_ -match '^(vcruntime|msvcp|concrt)' })
    if ($runtimeImports.Count -ne 1 -or $runtimeImports[0] -ne 'vcruntime140.dll') {
        throw "VC runtime imports changed; update the plugin payload: $runtimeImports"
    }

    # ④ 组装随包树（.sh 的 install -m 644/755 在这头就是 Copy-Item；执行位是 unix 的事）。
    $stamp = Get-Date -Format 'yyyyMMdd-HHmmss'
    $stage = Join-Path $root ".artifacts/plugins/notes-$stamp"
    $pluginRel = 'share/pingloss/plugins/notes'
    $pluginDir = Join-Path $stage $pluginRel
    New-Item -ItemType Directory -Path (Join-Path $pluginDir 'bin') -Force | Out-Null
    # ①' 清单：原样搬进随包那一格。
    Copy-Item -LiteralPath 'scripts/packaging/notes-plugin.json' -Destination (Join-Path $pluginDir 'plugin.json')
    # ②' 那颗产物按 `entry.path`＋平台后缀摆进 `bin/PinGloss-Note x Nomo.exe`。
    Copy-Item -LiteralPath $binary -Destination (Join-Path $pluginDir 'bin/PinGloss-Note x Nomo.exe')
    # ③' VC 运行库与 exe 并排（头上 ⚠：DLL 搜索先查 exe 自己那格目录）。
    $runtimeDirectories = @(Get-ChildItem -LiteralPath (Join-Path $env:VCToolsRedistDir 'x64') -Directory -Filter 'Microsoft.VC*.CRT')
    if ($runtimeDirectories.Count -ne 1) { throw 'Expected one x64 VC redistributable CRT directory.' }
    $runtime = Join-Path $runtimeDirectories[0].FullName 'vcruntime140.dll'
    if (-not (Test-Path -LiteralPath $runtime -PathType Leaf)) { throw 'Missing redistributable vcruntime140.dll' }
    Copy-Item -LiteralPath $runtime -Destination (Join-Path $pluginDir 'bin/vcruntime140.dll')

    # ④' AGPL 原文与组件许可随包：Linux 四份，Windows 多一份 windows-registry 的 MIT。
    Copy-Item -LiteralPath (Join-Path $root 'LICENSE') -Destination (Join-Path $pluginDir 'LICENSE')
    New-Item -ItemType Directory -Path (Join-Path $pluginDir 'licenses') -Force | Out-Null
    $licensePairs = @(
        @{ Source = 'src-gpui/assets/icons/LICENSE'; Name = 'LICENSE-Lucide' },
        @{ Source = 'src-gpui/assets/licenses/LICENSE-STIX'; Name = 'LICENSE-STIX' },
        @{ Source = 'src-gpui/assets/licenses/LICENSE-Rendering'; Name = 'LICENSE-Rendering' },
        @{ Source = 'src-gpui/assets/licenses/LICENSE-Export'; Name = 'LICENSE-Export' },
        @{ Source = 'src-gpui/assets/licenses/LICENSE-Windows'; Name = 'LICENSE-Windows' }
    )
    foreach ($pair in $licensePairs) {
        $source = Join-Path $root $pair.Source
        if (-not (Test-Path -LiteralPath $source -PathType Leaf)) { throw "缺组件许可原文 $source" }
        Copy-Item -LiteralPath $source -Destination (Join-Path $pluginDir ('licenses/' + $pair.Name))
    }

    # ⑤' 源码要约当趟生成：commit、产物 sha256、构建命令与工作区状态都取现场，不抄静态纸。
    $staged = Join-Path $pluginDir 'bin/PinGloss-Note x Nomo.exe'
    $commitResult = Invoke-Captured -File 'git' -Arguments @('rev-parse', 'HEAD')
    if ($commitResult.Exit -ne 0) { throw '读不出源码 commit' }
    $commit = ($commitResult.Stdout | Select-Object -First 1).Trim()
    if ($commit -notmatch '^[0-9a-f]{40}$') { throw "commit 不是 40 位十六进制：$commit" }
    $remoteResult = Invoke-Captured -File 'git' -Arguments @('remote', 'get-url', 'origin')
    $sourceUrl = if ($remoteResult.Exit -eq 0) { ($remoteResult.Stdout | Select-Object -First 1).Trim() } else { '' }
    $cargoText = Get-Content -LiteralPath (Join-Path $root 'Cargo.toml') -Raw -Encoding UTF8
    $versionMatch = [regex]::Match($cargoText, '(?m)^version\s*=\s*"([^"]+)"')
    if (-not $versionMatch.Success) { throw 'Cargo.toml 里读不出 Nomo 版本' }
    $nomoVersion = $versionMatch.Groups[1].Value
    $binSha = (Get-FileHash -LiteralPath $staged -Algorithm SHA256).Hash.ToLowerInvariant()
    $builtAt = (Get-Date).ToUniversalTime().ToString('yyyy-MM-ddTHH:mm:ssZ')
    $statusResult = Invoke-Captured -File 'git' -Arguments @('status', '--porcelain', '--untracked-files=normal')
    if ($statusResult.Exit -ne 0) { throw '读不出打包时的工作区状态' }
    $dirty = @($statusResult.Stdout | Where-Object { $_ -ne '' })
    if ($dirty.Count -gt 0) {
        $workspaceState = "**有 $($dirty.Count) 处未提交或未跟踪改动** ⇒ commit 记不下这一趟的改动，照构建命令重编**不保证**复现出同一个 sha256"
        Write-Warning "工作区有 $($dirty.Count) 处改动 ⇒ SOURCE.md 明写不保证复现。"
    } else {
        $workspaceState = '干净，就是上面那一枚 commit 的那棵树 ⇒ 照构建命令重编应能复现同一个 sha256'
    }
    $sourceHint = if ($sourceUrl) { "自己取：git clone $sourceUrl && git checkout $commit" } else { '这一份构建没有 origin remote，按下面索取方式走' }
    $pluginVersion = ((Get-Content -LiteralPath (Join-Path $pluginDir 'plugin.json') -Raw -Encoding UTF8) | ConvertFrom-Json).version
    $offer = @"
# notes 插件包的许可与源码声明

这一格是 PinGloss-Note x Nomo 的 notes 插件（清单＝同目录 plugin.json，可执行＝bin/PinGloss-Note x Nomo.exe），
由 Nomo 以插件模式构建。Nomo 采用 AGPL-3.0-or-later：原文见同目录 LICENSE，第三方组件许可见 licenses/。
按 AGPL-3.0 §13，随可执行文件交出的是完整对应源码，或下面这一纸指向源码的要约。

## 产物与源码对账

| 项 | 值 |
| --- | --- |
| 许可 | AGPL-3.0-or-later（LICENSE） |
| Nomo 产品版本 | $nomoVersion |
| 插件包版本 | $pluginVersion（与 Nomo 版本解耦） |
| 对应源码 | 下面那一枚 commit 的整棵仓库（含 src-gpui/ 源码、构建脚本、字体与图标资源） |
| commit | $commit |
| 源码地址 | $(if ($sourceUrl) { $sourceUrl } else { '这一份构建没有 remote 地址，按索取方式走' }) |
| 构建命令 | cargo build --locked --release --target x86_64-pc-windows-msvc --bin nomo |
| 产物 sha256 | $binSha |
| 打包那一下的工作区 | $workspaceState |
| 打包时间 | $builtAt（UTC） |

拿到 bin/PinGloss-Note x Nomo.exe 的人算一下 sha256 就能核对；只有工作区那一行写“干净”时，
sha256 与 commit 才一一对应。插件模式位于 src-gpui/src/plugin/，由宿主通过
PINGLOSS_PLUGIN_SOCKET／PINGLOSS_PLUGIN_DATA 两位环境变量启用；没有那两位就仍是桌面 Nomo。

## 索取方式与有效期

自本包发布起三年内有效（AGPL-3.0 §11 书面要约）：任何人可索取完整对应源码。

1. $sourceHint；
2. 致信仓库维护者（issue 或 README 上那个邮箱），说明要哪一枚 commit 的对应源码，以离线介质或公开地址交回，不收费（介质成本除外）。

Written offer valid for at least three years, in accordance with AGPL-3.0 section 11 — the complete
corresponding source is the repository at the commit above.
"@
    $sourcePath = Join-Path $pluginDir 'SOURCE.md'
    [IO.File]::WriteAllText($sourcePath, $offer, (New-Object Text.UTF8Encoding $false))

    # ⑥ 自证①：清单读得动、十一位必填齐备，七条 op 在 runtime handler 里各有落点。
    $manifestText = Get-Content -LiteralPath (Join-Path $pluginDir 'plugin.json') -Raw -Encoding UTF8
    $m = $manifestText | ConvertFrom-Json
    $required = @('id', 'kind', 'version', 'protocol', 'min_host', 'entry',
                  'capabilities', 'permissions', 'operations', 'license', 'signature')
    $missing = @($required | Where-Object { $m.PSObject.Properties.Name -notcontains $_ })
    if ($missing) { throw "清单缺必填位：$($missing -join ', ')" }
    if ($m.id -ne 'notes') { throw "id 应为 notes，读到 $($m.id)" }
    if ($m.kind -ne 'ui') { throw "kind 应为 ui，读到 $($m.kind)" }
    if ($m.display_name -ne 'PinGloss-Note x Nomo') { throw "display_name 应为 PinGloss-Note x Nomo，读到 $($m.display_name)" }
    if ($m.protocol -ne 1) { throw "protocol 应为 1，读到 $($m.protocol)" }
    if ($m.version -ne '0.3.1') { throw "version 应为 0.3.1，读到 $($m.version)" }        # 与 Nomo 版本解耦（§5.4②）
    if ($m.min_host -ne '0.9.597') { throw "min_host 应为 0.9.597，读到 $($m.min_host)" }  # 转发 notes.rename 的那一下限
    $entryProps = @($m.entry.PSObject.Properties.Name)
    if ($entryProps.Count -ne 1 -or $entryProps[0] -ne 'path' -or $m.entry.path -ne 'bin/PinGloss-Note x Nomo') {
        throw ('entry 应为 {"path": "bin/PinGloss-Note x Nomo"}（D-D①：联名可执行名，.exe 只由 Windows 落盘补），读到 ' + ($m.entry | ConvertTo-Json -Compress))
    }
    if ($null -ne $m.signature) { throw 'signature 应为 null（随包内置由安装包整体校验兜底）' }
    $ops = @($m.operations | ForEach-Object { $_.id })
    $expectedOps = @('folders.snapshot', 'notes.open', 'notes.list', 'note.new', 'notes.rename', 'note.delete', 'notes.info')
    if (($ops -join ',') -ne ($expectedOps -join ',')) { throw "七条 op 名册不齐或次序不对：$($ops -join ', ')" }
    $runtimeText = Get-Content -LiteralPath (Join-Path $root 'src-gpui/src/plugin/runtime.rs') -Raw -Encoding UTF8
    # 先圈定 impl Engine，再按 handle 自身缩进取方法体，兼容签名换行与可见性修饰。
    $engineMatch = [regex]::Match($runtimeText, '(?ms)^impl Engine \{.*?^\}')
    if (-not $engineMatch.Success) { throw 'runtime.rs 里找不到 impl Engine 的完整落点' }
    $engineText = $engineMatch.Value
    $handleStart = [regex]::Match($engineText, '(?m)^(?<indent>[ \t]+)(?:pub(?:\([^)]*\))?\s+)?fn\s+handle\s*\(')
    if (-not $handleStart.Success) { throw 'runtime.rs 里找不到 Engine::handle 的完整落点' }
    $bodyStart = $engineText.IndexOf('{', $handleStart.Index + $handleStart.Length)
    if ($bodyStart -lt 0) { throw 'Engine::handle 的签名后找不到方法体' }
    $indent = $handleStart.Groups['indent'].Value
    $handleEnd = [regex]::Match($engineText.Substring($bodyStart), '(?m)^' + [regex]::Escape($indent) + '\}[ \t]*\r?$')
    if (-not $handleEnd.Success) { throw 'Engine::handle 的方法体没有按自身缩进收口' }
    $handleText = $engineText.Substring($handleStart.Index, $bodyStart + $handleEnd.Index + $handleEnd.Length - $handleStart.Index)
    foreach ($op in $expectedOps) {
        if (-not [regex]::IsMatch($handleText, '"' + [regex]::Escape($op) + '"\s*=>')) {
            throw "runtime handler 里没有 $op 的派发落点"
        }
    }
    Write-Output "[清单] plugin.json 十一位齐＋display_name 在场、version=$($m.version)、min_host=$($m.min_host)、七条 op 与 runtime handler 一致"

    # ⑥ 自证②：那颗产物独立手起 `--probe` 要吐一份自述 JSON、退 0（不碰显示、不碰数据面）。
    # 没有宿主 spawn 的两格 env ⇒ `data_socket`／`data_dir` 该是 null，不是错——装机的人据此分得清「没装好」。
    $staged = Join-Path $pluginDir 'bin/PinGloss-Note x Nomo.exe'
    $probe = Invoke-Captured -File $staged -Arguments @('--probe')
    if ($probe.Exit -ne 0) { throw "--probe 退 $($probe.Exit)（应为 0）" }
    if ($probe.Stderr.Count -ne 0) { $probe.Stderr | ForEach-Object { Write-Warning $_ } }
    $d = (($probe.Stdout -join "`n").Trim()) | ConvertFrom-Json
    if ($d.app -ne 'nomo' -or $d.plugin_id -ne 'notes') { throw "自述身份不对：$($d | ConvertTo-Json -Compress)" }
    if ($d.display_name -ne 'PinGloss-Note x Nomo') { throw "自述联名不对：$($d | ConvertTo-Json -Compress)" }
    if ($d.mode -ne 'probe' -or $d.protocol -ne 1) { throw "自述模式/协议不对：$($d | ConvertTo-Json -Compress)" }
    if ($null -ne $d.data_socket -or $null -ne $d.data_dir) { throw "独立手起时 data_socket/data_dir 应为 null：$($d | ConvertTo-Json -Compress)" }
    Write-Output "[探针] bin/PinGloss-Note x Nomo.exe --probe 交回联名自述、退 0（Nomo 版本 $($d.version)）"

    # ⑦ 自证③：许可树齐备，SOURCE.md 的 commit／sha256／工作区／构建命令与当趟读数逐项对账。
    $licenseHead = (Get-Content -LiteralPath (Join-Path $pluginDir 'LICENSE') -Encoding UTF8 -TotalCount 1)
    if ($licenseHead -notmatch 'GNU AFFERO GENERAL PUBLIC LICENSE') { throw "LICENSE 首行不像 AGPL 原文：$licenseHead" }
    foreach ($pair in $licensePairs) {
        $licensePath = Join-Path $pluginDir ('licenses/' + $pair.Name)
        if ((Get-Item -LiteralPath $licensePath).Length -le 0) { throw "组件许可为空：$($pair.Name)" }
    }
    $offerText = Get-Content -LiteralPath $sourcePath -Raw -Encoding UTF8
    foreach ($needle in @($commit, $binSha, $workspaceState, '--bin nomo', '三年', 'src-gpui/src/plugin')) {
        if (-not $offerText.Contains($needle)) { throw "SOURCE.md 里读不到当趟读数：$needle" }
    }
    $actualSha = (Get-FileHash -LiteralPath $staged -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($actualSha -ne $binSha) { throw "SOURCE.md 的 sha256 与产物不符：$binSha vs $actualSha" }
    if ($m.license.required -ne $false -or $m.license.trial_days -ne 0) { throw '清单 license 应保持 required=false、trial_days=0' }
    Write-Output "[许可] AGPL 原文＋五份组件许可＋SOURCE.md（commit $($commit.Substring(0,9))、sha256 现场复核）"

    Write-Output "Nomo notes 插件随包树: $pluginDir"
} finally {
    Pop-Location
}
