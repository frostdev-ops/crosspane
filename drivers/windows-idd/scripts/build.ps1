# WP-W3.1a fixed unsigned static build. First execution is LEAD-only after ROOT review.
# Default help is inert. No installs, certificates, signing, deployment, restore,
# acquisition, arbitrary paths/properties/targets, native test or driver execution.
[CmdletBinding()]
param([ValidateSet('help', 'preflight', 'build', 'package')][string]$Mode = 'help')
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
if ($Mode -eq 'help') {
    Write-Output 'Crosspane IDD: default help launches nothing; -Mode preflight checks fixed local inputs only; explicit -Mode build is for LEAD after ROOT script review and verifies the ROOT-reviewed native source pins. -Mode package retains every static gate then generates one unsigned CAT with a separately sealed local Inf2Cat; unsealed tools refuse before build.'
    return
}
# LEAD f4d76209: plain foreground only. No internal timeout/process helper.
# External timeout or SSH loss is unresolved STOP; cleanup belongs to LEAD.
if ($env:OS -ne 'Windows_NT' -or -not [Environment]::Is64BitProcess) { throw 'Requires the fixed Windows x64 LEAD build environment.' }
$PackageRoot = 'C:\Users\Public\crosspane-w31a-pkgs'
$VsRoot = 'C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools'
$VcRoot = $VsRoot + '\MSBuild\Microsoft\VC\v170'
$ToolsRoot = $VsRoot + '\VC\Tools\MSVC\14.44.35207'
$Msbuild = $VsRoot + '\MSBuild\Current\Bin\amd64\MSBuild.exe'
$WdkRoot = $PackageRoot + '\Microsoft.Windows.WDK.x64.10.0.26100.6584\c'
$SdkRoot = $PackageRoot + '\Microsoft.Windows.SDK.CPP.10.0.26100.6584\c'
$SdkLibRoot = $PackageRoot + '\Microsoft.Windows.SDK.CPP.x64.10.0.26100.6584\c'
$DriverRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$RepoRoot = [IO.Path]::GetFullPath((Join-Path $DriverRoot '..\..'))
$Project = Join-Path $DriverRoot 'CrosspaneIdd.vcxproj'
if ($DriverRoot -match '[;"%$\r\n]' -or -not $DriverRoot.EndsWith('\drivers\windows-idd', [StringComparison]::OrdinalIgnoreCase)) { throw 'Invalid owned checkout layout.' }
# ROOT native annotation review c4f38644 under LEAD 20f53cc8/266ad02e passed checkpoint 2015bff4eb0a726436a625ee76f5dc2f3fa8add7ca86ecd79df27b9ccceb9136.
# Source byte integrity only, not runtime/load authority. No CLI value changes this
# table. First MSBuild remains LEAD-only after independent ROOT script review.
$NativeSourceReviewed = $true
$NativePins = [ordered]@{
    'src\Driver.h' = '07093781456ba4f8756005865c51640ba87fc93dc14531501672d0e9f3744862'
    'src\Driver.cpp' = '85311896d8cfdd60497ee2d32543a733f12283a2d6fba5ee76deac3359c92366'
    'src\Control.h' = '19a915da28419f86c5fd5565520faea8b5cfd4971bea48aa20cc4da6237a6a34'
    'src\Control.cpp' = '3300f11a193bc7b8c543d7fac7f1ba9782818352fb1cd685663ddbcc3276b67f'
    'src\Monitor.h' = 'bcb0d36151b3a28c6aca484594724266a6117a76f64a4d838e02b3b519bc7042'
    'src\Monitor.cpp' = 'cf29a29c0b7c2e0a3ac1e4ae99ebb594a9b0f6b4bd5bbf991bdd0ec767064ace'
    'src\Trace.h' = '0f95be57532cf0af5a9cd6ebd73e65dc89fef667ec0f4998d03a85d7e3872650'
}
$RequiredNativeRoles = @('src\Driver.cpp','src\Driver.h','src\Control.cpp','src\Control.h','src\Monitor.cpp','src\Monitor.h','src\Trace.h')
$Archives = @(
    @('microsoft.windows.wdk.x64.10.0.26100.6584.nupkg', 110872506, '8e175d6819e1303aaddc656bdf64554ed691d0a1e66438d8d09093327d74390a64e3e01285708af50034f6f05ace9f278b5323bbce2a3394865df21f9f2389fa'),
    @('microsoft.windows.sdk.cpp.x64.10.0.26100.6584.nupkg', 52245405, 'fb913010bc0ebec4b3806ac70d0d2cb5d68eb5864719f27d72fc7d6cde83f3c2b3394f892ec14bd5b10a1382bb53491df7daef6bfbafd4fe5a0ef41644283b39'),
    @('microsoft.windows.sdk.cpp.10.0.26100.6584.nupkg', 160036542, '2ab1d73514f4b2bdc1aa6bd5062af467f72bd48eba32f999e984f005ace2d13cfb3f7e7ab91082ca78e5dffad9c2c4b424b602743ffe7e54d2375d35adc9a6fa')
)
# Root-reviewed Microsoft text-role integrity pins; no package restore or arbitrary path input.
$MicrosoftPins = @(
    @('packages', 'Microsoft.Windows.SDK.CPP.10.0.26100.6584\build\ExpandTargetPlatform.targets', '22cf3f56dbfeffa2643ea1a3dd7ae9f219a5f90984f329949121d76dfc0d41d3'),
    @('packages', 'Microsoft.Windows.SDK.CPP.10.0.26100.6584\build\Microsoft.Windows.SDK.cpp.props', 'befc9607909affc22d8f2bfb3924d4222e22ffc9e4d7424720ea587cea6eb71e'),
    @('packages', 'Microsoft.Windows.SDK.CPP.10.0.26100.6584\build\Microsoft.Windows.SDK.cpp.targets', 'b17e65a523318970a1884ca22620373b107f1b59b4722473fe1df3d7d702529c'),
    @('packages', 'Microsoft.Windows.SDK.CPP.10.0.26100.6584\build\Override.Current.targets', '630524c41ecf4190d2ccaac414f0853f052f206fce791336a06272752e64a8af'),
    @('packages', 'Microsoft.Windows.SDK.CPP.10.0.26100.6584\build\TargetVersion.props', 'e7c59365662e2accdbbc09318e7e5e0925e315e779f407a6854a8f489d659fea'),
    @('packages', 'Microsoft.Windows.SDK.CPP.10.0.26100.6584\build\native\Microsoft.Windows.SDK.cpp.props', 'd1d3f662e5b9cf72fa15e581e5a255b855294de5e834005201ed716e3cc201f9'),
    @('packages', 'Microsoft.Windows.SDK.CPP.10.0.26100.6584\build\native\Microsoft.Windows.SDK.cpp.targets', '7c27a6e71dc5c0a88d07d877717bb0508957f794ce658d181544801a124b012e'),
    @('packages', 'Microsoft.Windows.SDK.CPP.x64.10.0.26100.6584\build\native\Microsoft.Windows.SDK.cpp.x64.props', '673f23675b5153ee96805a56d8a73342fc6b53363644fb35b1bd0cf6c63520dd'),
    @('packages', 'Microsoft.Windows.WDK.x64.10.0.26100.6584\build\native\Microsoft.Windows.WDK.x64.props', '40058eb63f4bf1a0452f8055dbeb6ff4c0e8f69f68e41e1509e7d18d9e5d1a44'),
    @('vc', 'Microsoft.Cpp.Common.props', 'f3e9fe63324e7152fe2a3aa2735198479cf9ece496624383a49827355f428978'),
    @('vc', 'Microsoft.Cpp.Default.props', 'b57aee14de985d933d18e129042c91932ea9250252c8f756d6d774050e72ef92'),
    @('vc', 'Microsoft.Cpp.Platform.props', '149c0c5a5fa2e8dc52bf3265dde0c0a7297dd9f25a5d97d569d4884954f6aaf2'),
    @('vc', 'Microsoft.Cpp.Platform.targets', '7f05c0b6be336038768ba7f923a3b2c157c6830b1ebc062673819e191ebb99db'),
    @('vc', 'Microsoft.Cpp.props', 'b4c3067fa4394955831f7a3d6b65833d0bbb61fb462484351bb556724420e9c8'),
    @('vc', 'Microsoft.Cpp.targets', '1ce9adb7c77483042f5e941ff7fdab0153c23e219f6e6cf25062655b174490ba'),
    @('vc', 'Platforms\x64\Platform.Common.props', '9604400f8fca3d29a46d4c74f7b9c30044a914078a9aff6ae6f1108f5e8d967e'),
    @('vc', 'Platforms\x64\Platform.Default.props', '96ba1f87fb0fbddc6a5a56c219ab05c8bcd36fe6a77e7963c675acdaf0dd75fc'),
    @('vc', 'Platforms\x64\Platform.props', '33148ae25c6886ec16ae09bdb1e14b0772c9f74652afc9b9a0bf0303229a5c35'),
    @('vc', 'Platforms\x64\Platform.targets', '99fc7c74f0e8c0658edf82d4003ad50f9e0317d3e0bf8c57c55fb713814f05ff'),
    @('vc', 'Platforms\x64\PlatformToolsets\v143\Toolset.props', '45927a0697ed83cc6e5f62bae0c9c9865a486c788471cfe095b5f0228159776e'),
    @('vc', 'Platforms\x64\PlatformToolsets\v143\Toolset.targets', '4633e099b31c0a8a3c937fdf06d287e7c00e20e7317381a8563bbd1ced9717c5'),
    @('wdk', 'DesignTime\CommonConfiguration\Neutral\WDK\10.0.26100.0\WDK.props', 'd27af8ace9c7df7d108e00c8b75c7bdf9a0eb4bf1e9c6c0813eed1a5d8df222b'),
    @('vc', 'Platforms\x64\ImportAfter\Microsoft.Cpp.WDK.props', '49757d30227e295d42ac8d3d7d08a451f290cb15504737f471813f2c78204047'),
    @('vc', 'Platforms\x64\ImportAfter\Microsoft.Cpp.WDK.targets', '058d1735b1a34afea385a9dfeaf816ee84d0818c6d91be52e71fb1ee85176294'),
    @('vc', 'Platforms\x64\ImportBefore\Default\Microsoft.Cpp.WDK.props', 'e426208e1e3cab0b05d41a6a888c9bf409fad79969f3401fad89b7befd45064b'),
    @('vc', 'Platforms\x64\ImportBefore\Microsoft.Cpp.WDK.props', '12221069829069a75ce146236c193cfc3da52804eda5953ee5db59034bb38f52'),
    @('vc', 'Platforms\x64\PlatformToolsets\WindowsApplicationForDrivers10.0\Toolset.props', '2b68c0cdc82725b25f117fef207805f5a3e8d5ce0de0c92ba8e5bc9c8f758591'),
    @('vc', 'Platforms\x64\PlatformToolsets\WindowsApplicationForDrivers10.0\Toolset.targets', '8224ebe4dc967be99cf4569febf03b45aba2d94ed908be4b818ea9f7c28075a4'),
    @('vc', 'Platforms\x64\PlatformToolsets\WindowsKernelModeDriver10.0\Toolset.props', 'ae3369a8183b95ffcfa8bddaa183279fc8b0d2dd6368c21881678399660dc2b2'),
    @('vc', 'Platforms\x64\PlatformToolsets\WindowsKernelModeDriver10.0\Toolset.targets', '348a2e67597fe3df57d9f77212cbd06cacbff38edbdcf0c19a08b64a96bd5b27'),
    @('vc', 'Platforms\x64\PlatformToolsets\WindowsUserModeDriver10.0\Toolset.props', 'a9bb227767ac8e5aec09c5a7e6ae32d098e076c4d3094123916e7be40be6e808'),
    @('vc', 'Platforms\x64\PlatformToolsets\WindowsUserModeDriver10.0\Toolset.targets', '7651efbe90966a9e0e501f7737cf76e57940322a06dfba8b8be7dd41a9e0cbe7'),
    @('wdk', 'Include\10.0.26100.0\um\iddcx\1.10\IddCx.h', 'a436ff095e47b846283d4acdd8b9d03c9bfadc9c51c1b4c6a161eb0bf9fee518'),
    @('wdk', 'Include\10.0.26100.0\um\iddcx\1.10\IddCxFuncEnum.h', '86dec0cd5f766fbb19642eb5218cd9682b47032db1af261f7297b9f73ddfecc2'),
    @('wdk', 'Include\10.0.26100.0\um\iddcx\1.10\IddCxTraceEnums.h', 'd7176824b1516ad55107cb322614866a8b643546b28a1530c52c8e90d0811f16'),
    @('wdk', 'Include\wdf\umdf\2.25\wdf.h', '1b0bb635469c50395d13182fe1d9888b09e5de495af5b7edd92bb4a156427358'),
    @('wdk', 'Include\wdf\umdf\2.25\wdfassert.h', '8a840687986e417dcd96587b36a02179e3e36c852bf80d679af0e04d7a283e84'),
    @('wdk', 'Include\wdf\umdf\2.25\wdfbugcodes.h', '4c065800eef22c6d6e31cb837ab05a6ac94912aa49d84c7b1ad90b5c370ae40e'),
    @('wdk', 'Include\wdf\umdf\2.25\wdfchildlist.h', 'abdd0acf060fcba714a8e76e1fca4d9ed060c477ecc75306053682cbb8f4b27f'),
    @('wdk', 'Include\wdf\umdf\2.25\wdfcollection.h', '2348a7da3ffe0c8fd9152698187722d622e2ff3c10b9153f5a66635ba541859f'),
    @('wdk', 'Include\wdf\umdf\2.25\wdfcompanion.h', 'f303609492d38bba44a5792f67ac4579e45853a3c526c51297775cd03097b02a'),
    @('wdk', 'Include\wdf\umdf\2.25\wdfcontrol.h', 'a296d1cf969f121ee1011067d3f3e9c2e5d5e72b47738708dc4cc241ef52e6ad'),
    @('wdk', 'Include\wdf\umdf\2.25\wdfcore.h', '677efd6c3c178a6dbd232ed48d09425d6dd7a4904b80e3d5738228c6275bf24f'),
    @('wdk', 'Include\wdf\umdf\2.25\wdfdevice.h', 'fb4382f3506d055c88b9d08e47ef2746afbac252bd04eaa3e4e0dc3154309402'),
    @('wdk', 'Include\wdf\umdf\2.25\wdfdriver.h', '1273ebb3a0113e939fdecb73038880749220114bb0fd926a6e484f9f31acc9ab'),
    @('wdk', 'Include\wdf\umdf\2.25\wdffdo.h', 'dfd85a92cfa8f43f84b7fe6cbf638347196e6e847e86204cbcd470930bee49ef'),
    @('wdk', 'Include\wdf\umdf\2.25\wdffileobject.h', 'd7df94aa35ba0a94bba805d144b3f45dcc9b8fc7b137da902c09804304d19628'),
    @('wdk', 'Include\wdf\umdf\2.25\wdffuncenum.h', '3f670959c412f34a7ac009bf2b892acc5feaeecec4f02e1c837717dad4dc9a86'),
    @('wdk', 'Include\wdf\umdf\2.25\wdfglobals.h', '7a4775e627977374722ee2243d2336b221ceb7e62397d3b4d9240e26a04c6562'),
    @('wdk', 'Include\wdf\umdf\2.25\wdfhwaccess.h', 'c0b0571b0ed80bf2f0a49b68ce97dfdaacf6a5b906f4698f4efb3885a7d3f4f0'),
    @('wdk', 'Include\wdf\umdf\2.25\wdfinterrupt.h', '48f753c7c850ac368da79fdf53e63e71d2be70175f87f3b002e223f9aca50ad2'),
    @('wdk', 'Include\wdf\umdf\2.25\wdfio.h', '85b886583892b4ea42396278c795d39a4d85571447d928eb1a8558783041fa4d'),
    @('wdk', 'Include\wdf\umdf\2.25\wdfiotarget.h', 'd4a089ad710c825823041931abd5428bb65cc1a3306758162c80522da07f860f'),
    @('wdk', 'Include\wdf\umdf\2.25\wdfmemory.h', 'a43810a1745b95bd3db7333f6a6f784a6518b4968a4d38d479ea4cbf55487122'),
    @('wdk', 'Include\wdf\umdf\2.25\wdfobject.h', '29d708d454f63b152403740c0ef164b8d0d08c64e05cec9a5c341133b2f13f4f'),
    @('wdk', 'Include\wdf\umdf\2.25\wdfpdo.h', '66299232380af2b77e4548b4c50ab445b3a6f88c05bd1bd03135c96860aafc7a'),
    @('wdk', 'Include\wdf\umdf\2.25\wdfqueryinterface.h', '61664286e104dbd619b38199f7427dc81b5db6897bb31dd5808b8fd34c2db25f'),
    @('wdk', 'Include\wdf\umdf\2.25\wdfregistry.h', 'f90369af52ed19acc2c5df2c1cda70550d9d9da881c3a8f8a2ed331db17b9a71'),
    @('wdk', 'Include\wdf\umdf\2.25\wdfrequest.h', 'ad2024b4f7ab9d8d397bd988ec31515fd6d019419dbea35a5edebc4eec121a0d'),
    @('wdk', 'Include\wdf\umdf\2.25\wdfresource.h', '6574eed1f480c22663c569e05872ebe7997733e14fb4309c5cc49b9448daf774'),
    @('wdk', 'Include\wdf\umdf\2.25\wdfroletypes.h', '7895e543d7967b8a60c82680123bb703234bb38766f9f1641b492363e9eb081f'),
    @('wdk', 'Include\wdf\umdf\2.25\wdfstatus.h', 'c97bda13ce22cbc777059175216ec7f06fc977413c82ac26588ba3574b12233d'),
    @('wdk', 'Include\wdf\umdf\2.25\wdfstring.h', '7d80dec803acea3ad2dc8d57d0a03228d6c9e0369a4518fc1dd9f6065b68db77'),
    @('wdk', 'Include\wdf\umdf\2.25\wdfsync.h', 'd7c108f63e27ee9b46564201e1c83420745bea6d744db1df5863b161ef005879'),
    @('wdk', 'Include\wdf\umdf\2.25\wdftimer.h', '50c453ce1f2dfd2c23477d69b7a2b5e3cec3d290cbb55c44a2a26399b010f54f'),
    @('wdk', 'Include\wdf\umdf\2.25\wdftriage.h', 'c1f9d862e6a39b6ed7053770e3c2b74618208c3918817d76becb89b45930b85b'),
    @('wdk', 'Include\wdf\umdf\2.25\wdftypes.h', '5016b6263385d2d46d6a5c5b25a16854ac2b7a07a751d703202ef14ffd6a8b75'),
    @('wdk', 'Include\wdf\umdf\2.25\wdfusb.h', 'd0a29cacd4aa528c8b27bdbeb734d2a328523dfbf4f2344cabfa9d50f6ab0c03'),
    @('wdk', 'Include\wdf\umdf\2.25\wdfverifier.h', '952d8efe61dbadf40b1c408ae6f9177684770947ba285e1b8e787a23ff6a33be'),
    @('wdk', 'Include\wdf\umdf\2.25\wdfwmi.h', '584650c1a832cd22774e66a901eef6a4d1808b770a072280c7a6e7a7880f86d4'),
    @('wdk', 'Include\wdf\umdf\2.25\wdfworkitem.h', '27da14653627ddc7d3d593ba13ec6c02c00d9b9b11c2d1bf29eafabafc7a003d'),
    @('wdk', 'Include\wdf\umdf\2.25\wudfwdm.h', 'f4be2ef28a5733f470f94f915826fc2a490ef581730f1a843f416faefeeaa070'),
    @('wdk', 'build\10.0.26100.0\ARM64\ImportAfter\Dbgeng.targets', 'ebdd641acf5d0eee912fb8b43e0ff7c3a4832c7a7bd4c5596694a8907317c1df'),
    @('wdk', 'build\10.0.26100.0\ARM64\ImportAfter\WDK.arm64.WindowsApplicationForDrivers.Platform.props', 'dea8e75912955e1f60aa5eb046f6e6e7f8028fa8f0e570637a5607d41892773a'),
    @('wdk', 'build\10.0.26100.0\ARM64\ImportAfter\WDK.arm64.WindowsKernelModeDriver.Platform.props', '4f40cc36f03ba0b29201ab3f033ab0698fb8a2ccadf557046b02eeeeeae1776c'),
    @('wdk', 'build\10.0.26100.0\ARM64\ImportAfter\WDK.arm64.WindowsUserModeDriver.Platform.props', 'efcdd1499ffdfd0e6375fc3c9c6a94ccb65242c5e6aa0a2730dbe5b0a6a2fe5c'),
    @('wdk', 'build\10.0.26100.0\ARM64\WindowsApplicationForDrivers\WDK.Arm64.WindowsApplicationForDrivers.props', '428eaa1013d039d0c004f697b7c986ae8d95005c41d7f889ed8ef36ed1ef7392'),
    @('wdk', 'build\10.0.26100.0\ARM64\WindowsKernelModeDriver\WDK.Arm64.WindowsKernelModeDriver.props', 'd3957e1e5dc1f93bb699f82a0f743e95e8fa0c0026647c712a2fe97b4dfbafe2'),
    @('wdk', 'build\10.0.26100.0\ARM64\WindowsUserModeDriver\WDK.Arm64.WindowsUserModeDriver.props', '92f8893481350e4e51a897dc8b3b3f93cf80d70f38964b9d2076945713650432'),
    @('wdk', 'build\10.0.26100.0\ARM64EC\ImportAfter\Dbgeng.targets', 'ebdd641acf5d0eee912fb8b43e0ff7c3a4832c7a7bd4c5596694a8907317c1df'),
    @('wdk', 'build\10.0.26100.0\ARM64EC\ImportAfter\WDK.arm64.WindowsApplicationForDrivers.Platform.props', 'dea8e75912955e1f60aa5eb046f6e6e7f8028fa8f0e570637a5607d41892773a'),
    @('wdk', 'build\10.0.26100.0\ARM64EC\ImportAfter\WDK.arm64ec.WindowsUserModeDriver.Platform.props', 'efcdd1499ffdfd0e6375fc3c9c6a94ccb65242c5e6aa0a2730dbe5b0a6a2fe5c'),
    @('wdk', 'build\10.0.26100.0\ARM64EC\WindowsApplicationForDrivers\WDK.Arm64EC.WindowsApplicationForDrivers.props', '4d6a88858b4d3316094367cf40f412113342e0a5c95d53e118029c87149a20b5'),
    @('wdk', 'build\10.0.26100.0\ARM64EC\WindowsUserModeDriver\WDK.Arm64EC.WindowsUserModeDriver.props', 'bd23b7b6d1de6023c8e10c5fcabcf833e760091095ddbb1ca4e236f06db138ac'),
    @('wdk', 'build\10.0.26100.0\Universal.ApplicationForDrivers.props', '7e3f7b34b1b50ae6d874009c254b85f461289fd2814dbda8e29a6e2b74737240'),
    @('wdk', 'build\10.0.26100.0\Universal.UserMode.props', 'c80c6c11e961ea36c4e00e7511b776c9b8dd1e2d0ab3bc9ed5eab6261dbcda26'),
    @('wdk', 'build\10.0.26100.0\Win32\ImportAfter\WDK.Win32.WindowsApplicationForDrivers.Platform.props', '9fbdcb2a4bb025448681277e4080bae89f837b36ba7b65fdcb4d92e1d76dc7dc'),
    @('wdk', 'build\10.0.26100.0\Win32\ImportAfter\WDK.Win32.WindowsKernelModeDriver.Platform.props', '2fc1f85baf23b4b1f982f16139bc950060e9f5b3d42fc223de3f86b9c4a928d2'),
    @('wdk', 'build\10.0.26100.0\Win32\ImportAfter\WDK.Win32.WindowsUserModeDriver.Platform.props', 'a0a10560f1b3a503f91d41c780d51b38f29b40d72d5b168ea49c9abf09719345'),
    @('wdk', 'build\10.0.26100.0\Win32\WindowsApplicationForDrivers\WDK.Win32.WindowsApplicationForDrivers.props', '4e33baf39d7d7924a7b93b88b6946cd9476d12bbfec1f4e9415023501562d887'),
    @('wdk', 'build\10.0.26100.0\Win32\WindowsKernelModeDriver\WDK.Win32.WindowsKernelModeDriver.props', '332fd8d8a101b13ec9b5571317366900c0a3d76353113e77e1701d55c200b107'),
    @('wdk', 'build\10.0.26100.0\Win32\WindowsUserModeDriver\WDK.Win32.WindowsUserModeDriver.props', 'c924705ad341ba6ffb0c2c7b84208d7c6bbdfe0fc9a4a88ab4aec6ae5fb2df76'),
    @('wdk', 'build\10.0.26100.0\Windows.UserMode.props', '99fab4794963dfff47c0cacec66fe0f6dd2279700dd6151305b7bdb22ba05e06'),
    @('wdk', 'build\10.0.26100.0\WindowsDriver.Common.props', '392b17940710c509c09282077b98dd66b48e7de4f915dddf6fb81e82ccd05ce5'),
    @('wdk', 'build\10.0.26100.0\WindowsDriver.Common.targets', 'b8adf19ea0617ceabc88aa4b14d751df2e944dd3bd0b067a5fe2a0330b9e88cf'),
    @('wdk', 'build\10.0.26100.0\WindowsDriver.Default.props', 'e2166cc99658192f7b5a8714f699c68b78af038baa1aea8343ad60c7db2db71a'),
    @('wdk', 'build\10.0.26100.0\WindowsDriver.KernelMode.CX.Default.props', 'c9d59def3476df68fa3b48b300c5161a16397bc1fcb5dc7e14dfac61641361d3'),
    @('wdk', 'build\10.0.26100.0\WindowsDriver.KernelMode.CX.LateEvaluation.props', '4e2164c792bd4fcd76fa61e10c9c0f1f94386e0432733f85b60f59fd8d7b0a6c'),
    @('wdk', 'build\10.0.26100.0\WindowsDriver.KernelMode.CX.targets', '9fe6530b904be1f4c95a3e4041b24d98e715c87a07fb1fe09a73285ba72a415f'),
    @('wdk', 'build\10.0.26100.0\WindowsDriver.KernelMode.Default.props', '085c72d5765dbc5c5e23af35a7cc2ef7f63ff3226f730a80efc738b5d6f6cfb1'),
    @('wdk', 'build\10.0.26100.0\WindowsDriver.KernelMode.Driver.props', '356dc5f60099e829626f5f14ed5ffbc6bf65f818b152a1231ebfd0ad321b83b0'),
    @('wdk', 'build\10.0.26100.0\WindowsDriver.KernelMode.ExportDriver.props', '545850ce2566c00563d626b9524786ed3189df01049c7c6e78ced8b0a2985483'),
    @('wdk', 'build\10.0.26100.0\WindowsDriver.KernelMode.Gdidriver.props', '743277508b182c122349c8b47913ca240664941a087643c45017159211563f0d'),
    @('wdk', 'build\10.0.26100.0\WindowsDriver.KernelMode.KMDF.props', 'b7758b6e2a50551e77165c849eaa92fbb73471c5fb208d6e638e3a692911ef7d'),
    @('wdk', 'build\10.0.26100.0\WindowsDriver.KernelMode.LateEvaluation.props', 'ce42e56d996bafcf5c89a874902ce55a94c2df7574a3a39bfb09a61be66ac5c3'),
    @('wdk', 'build\10.0.26100.0\WindowsDriver.KernelMode.Miniport.props', 'a0ca29382d5bfd556d520a5729f6cb9572df90ce50c11215126645ea38efc704'),
    @('wdk', 'build\10.0.26100.0\WindowsDriver.KernelMode.Wdm.props', '4eee270078d7ba9d5e47c40d8c12e9ea405f2e6dcf5835a8585fc3f4c34fed2e'),
    @('wdk', 'build\10.0.26100.0\WindowsDriver.KernelMode.props', 'c6896a48df49798807f03affc79c424571f8900814a5df6a2c380afd7d3a832b'),
    @('wdk', 'build\10.0.26100.0\WindowsDriver.LateEvaluation.props', 'b8ad498ffb4ba2ab2cebefc9e7c40f5f6302584cd5f615bc8a1d6be8ed5eeb57'),
    @('wdk', 'build\10.0.26100.0\WindowsDriver.OSTargets.props', 'a0ed3be88186e22a058a6cb090750531ddd302abf9ac092208690f2c9810d89a'),
    @('wdk', 'build\10.0.26100.0\WindowsDriver.Sdv.targets', '536bab61b30bf2c3882ffd89307b44d59ec3fa22742f58287d720f6dfe3e3132'),
    @('wdk', 'build\10.0.26100.0\WindowsDriver.UserMode.CX.Default.props', '345246242fd60cb849211e499b59058e1d7d0a0d0a46cc1e7a323ed9869a6b15'),
    @('wdk', 'build\10.0.26100.0\WindowsDriver.UserMode.CX.LateEvaluation.props', 'f545bf20d5e6f22b1d47197e1ebd089e555eeb1525e2091b52d021804ce7af37'),
    @('wdk', 'build\10.0.26100.0\WindowsDriver.UserMode.CX.targets', 'e7ad89ca6f2478bf628453c2c38f8c237170c9b88f085ecf83545844b1aad700'),
    @('wdk', 'build\10.0.26100.0\WindowsDriver.UserMode.Default.props', '361b28e46158e8e612337b2f244cd7c130f0e40b7ca00cd4d4bee6f51eb560dc'),
    @('wdk', 'build\10.0.26100.0\WindowsDriver.UserMode.LateEvaluation.props', '46e71f9ce19a3ce9c39a67a32489c1dc3a117e26a4da85d77ca7291338b89929'),
    @('wdk', 'build\10.0.26100.0\WindowsDriver.UserMode.UMDF.props', '95aa7339540556ff394fb0a9496b544fa9db722069e3dba810c36f55cf0661d7'),
    @('wdk', 'build\10.0.26100.0\WindowsDriver.UserMode.props', 'a4269d47497395c922d487c203aed47a5a995a12a5b245ea588e78aa3e274dcb'),
    @('wdk', 'build\10.0.26100.0\WindowsDriver.Win32.props', '2268c133c3442791069f9c134ea82a2a4e3b82ed5f94e43de013fe307e23aef3'),
    @('wdk', 'build\10.0.26100.0\WindowsDriver.arm64.props', '08f0d2fcb47a255bf3f748cc337a06dc182d67739cffbb05819e64ad086ca8fa'),
    @('wdk', 'build\10.0.26100.0\WindowsDriver.arm64.targets', '234ee5300d6050f86141dfe9db4280a6cd079017b0149d2a3bda64601ac527f2'),
    @('wdk', 'build\10.0.26100.0\WindowsDriver.arm64EC.props', '68cbca5e5b831b8f6ac2d1f1c3179cc319f44b3959f12ec515f1100362ccac7b'),
    @('wdk', 'build\10.0.26100.0\WindowsDriver.arm64EC.targets', 'e782aef2a78a0bc61fe334e0155c9e5750a010ea82c3b7a9b618f7b929cafdad'),
    @('wdk', 'build\10.0.26100.0\WindowsDriver.masm.props', '4ea6353c136d7385d869f091341d320a1688ea8daff66afefa1a842e76f82b2f'),
    @('wdk', 'build\10.0.26100.0\WindowsDriver.masm.targets', '4e992455dd272af8b500fb4e0afd57b75e6e569352ed2abd48bd79b5061ff4ee'),
    @('wdk', 'build\10.0.26100.0\WindowsDriver.x64.props', '4e99611e7d69082451a1a94a89c725e9048b890e5f61f1ccee5538d389f7c52a'),
    @('wdk', 'build\10.0.26100.0\WindowsPhone.WindowsApplicationForDrivers.props', '6d2c1b5fa1b2764cf939aff824ca8a1255e1b1314c3a9af0c4ec9ce5c147a967'),
    @('wdk', 'build\10.0.26100.0\WindowsPhone.WindowsUserModeDriver.props', '863782d1988971c45a3a7d2ae9598ca88a259890f6a9542a9d4ccb03a3035d46'),
    @('wdk', 'build\10.0.26100.0\x64\ImportAfter\WDK.x64.WindowsApplicationForDrivers.Platform.props', '9fbdcb2a4bb025448681277e4080bae89f837b36ba7b65fdcb4d92e1d76dc7dc'),
    @('wdk', 'build\10.0.26100.0\x64\ImportAfter\WDK.x64.WindowsKernelModeDriver.Platform.props', '2fc1f85baf23b4b1f982f16139bc950060e9f5b3d42fc223de3f86b9c4a928d2'),
    @('wdk', 'build\10.0.26100.0\x64\ImportAfter\WDK.x64.WindowsUserModeDriver.Platform.props', 'a0a10560f1b3a503f91d41c780d51b38f29b40d72d5b168ea49c9abf09719345'),
    @('wdk', 'build\10.0.26100.0\x64\WindowsApplicationForDrivers\WDK.x64.WindowsApplicationForDrivers.props', '5e45289d2ba9cd6711771572171d57e1811648807aa77cdf59cf9d354081f892'),
    @('wdk', 'build\10.0.26100.0\x64\WindowsKernelModeDriver\WDK.x64.WindowsKernelModeDriver.props', '28c4951b6af2aacf2a28239d83bff78f38ce96d13261d523dd6ee1f9ee6885c3'),
    @('wdk', 'build\10.0.26100.0\x64\WindowsUserModeDriver\WDK.x64.WindowsUserModeDriver.props', '9f8464b349afe9148ac7bea971fccb54809137f6acfced82b2fa7afafed35d5a'),
    @('wdk', 'build\WindowsDriver.Common.targets', 'f22ac354f7f85952409fb3337aa6d6a6425de046ccded9f907f20366c3aa3000')
)
$SourcePins = [ordered]@{
    'include\crosspane_idd_v1.h' = 'f60bbc0e64249f4812e8088461a7ec00b1349abe2667f033172967a62034f38b'
    'src\Lease.h' = '9ed6c7f45f9c87ee94bb3fd949d3f88dc74095b7fd8f6683a3b15bee94b3fabd'
    'src\Lease.cpp' = 'fc418a97817921eccc68d02b6d2890c07330812efaf92b2b882537c19214aef2'
    'src\Edid.h' = '3cc0c844acf44d62f19b7c387ff18a800b11335ea1aba997211d1fc12009ea7f'
    'src\Edid.cpp' = '59724061cca9eb6314b97238ca166013baa7df83038b8799d292d7a76489efae'
    'tests\abi_lease_tests.cpp' = 'cd951d37431939c317204a4eb86bbeb5d49b75640a1842782c9b67c135283bf8'
    'CrosspaneIdd.inf' = 'd733de171c08ffef83cfecc492381d5329963dfeb0f6f345777e80645bbec714'
    'CrosspaneIdd.vcxproj' = '600fb903d4132f0b8db04941338baaa0ad93ffb1a809b70eb2d6d7e8fe6fc493'
    'Directory.Build.props' = '1207d385750c951b000635f2549c357ba07b876836d8059732bf0d83ee8ae5e0'
}
function Require-File([string]$Path) {
    if (-not [IO.File]::Exists($Path)) { throw ('A fixed input file is missing: ' + $Path) }
    $check = [IO.Path]::GetFullPath($Path)
    while ($check) {
        $item = Get-Item -LiteralPath $check -Force
        if (($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) { throw 'A fixed input path contains a reparse point.' }
        $parent = [IO.Directory]::GetParent($check)
        if ($null -eq $parent) { break }
        $check = $parent.FullName
    }
}
function Require-Hash([string]$Path, [string]$Algorithm, [string]$Expected) {
    Require-File $Path
    if ((Get-FileHash -LiteralPath $Path -Algorithm $Algorithm).Hash -ine $Expected) { throw ('Fixed input byte integrity mismatch; no fallback: ' + $Path) }
}
# LEAD0ffe2a0a: exact Microsoft-signed static tool from the pinned WDK6584 package.
$InfVerif = Join-Path $WdkRoot 'tools\10.0.26100.0\x64\infverif.exe'
$InfVerifSha256 = '859e311fc5fcdbc041750e50f3673258cbf90f46dbb170c27d7f9fc4f2afdf3a'
# W3.1a2 GO15e53e04: catalog generation is an explicit foreground post-static phase.
# LEAD b24feb0b: exact staged SHA512-provenanced6584 managed tool and seven-file closure.
# Internal Microsoft signing reports UnknownError; Valid is NOT required for Inf2Cat.
# Hash/package identity is the frozen gate; no ambient member or timestamp/network fallback.
$Inf2CatDirectory = Join-Path $WdkRoot 'bin\10.0.26100.0\x86'
$Inf2Cat = Join-Path $Inf2CatDirectory 'Inf2Cat.exe'
$Inf2CatPackageReviewed = $true
$Inf2CatPackageSha256 = 'b594728d38b271979367abc8060a971b8e42422738009be126710b1f5dd0fcbc'
$Inf2CatPackageBytes = 34880
$Inf2CatClosure = @(
    @('Inf2Cat.exe',34880,'b594728d38b271979367abc8060a971b8e42422738009be126710b1f5dd0fcbc'),
    @('Microsoft.UniversalStore.HardwareWorkflow.Cabinets.dll',60480,'0e6cffc7b944b1357a7fefe6fce63221462ca06eaef3d4cad73adc6d19f6b290'),
    @('Microsoft.UniversalStore.HardwareWorkflow.Catalogs.dll',33344,'7117e47751b1847bb7689275877494a1e3f3f89ccbaaba0d8c5f129b33b897d5'),
    @('Microsoft.UniversalStore.HardwareWorkflow.InfReader.dll',60504,'eb390ce8b00fde9240351de0ad4220885e5f8c0f23ed9e49442dfbee76c27753'),
    @('Microsoft.UniversalStore.HardwareWorkflow.SubmissionBuilder.dll',144448,'3f3bfa00c54420bd5d4db1e863b13a4029e0b4949c3504e13a9b412ae5842a18'),
    @('WindowsProtectedFiles.xml',180884,'b6dcf5d577c4eb96a63dba8e8f952cc9a1b6e7c21c6c5784aec82b261abec640'),
    @('aitstatic.exe',3033432,'9d9b99107d5fa9753ce26cd5b5430a07bc8845d230df952f9825bde5dda5024d')
)
$Inf2CatHeld = [Collections.Generic.List[IDisposable]]::new()
if ($Mode -eq 'package') {
    if (-not $Inf2CatPackageReviewed -or $PSVersionTable.PSEdition -cne 'Desktop' -or [Environment]::Version.Major -ne 4) { throw 'STOP: reviewed6584/.NET4 catalog runtime unavailable.' }
    foreach ($entry in $Inf2CatClosure) {
        $path = Join-Path $Inf2CatDirectory $entry[0]
        Require-File $path
        $ancestor = $path
        while ($ancestor) {
            if (((Get-Item -LiteralPath $ancestor -Force).Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) { throw 'Catalog tool reparse ancestor refused.' }
            $parent = [IO.Directory]::GetParent($ancestor)
            if ($null -eq $parent) { break }
            $ancestor = $parent.FullName
        }
        $stream = [IO.File]::Open($path,[IO.FileMode]::Open,[IO.FileAccess]::Read,[IO.FileShare]::Read)
        $digest = [Security.Cryptography.SHA256]::Create()
        try {
            if ($stream.Length -ne $entry[1] -or [BitConverter]::ToString($digest.ComputeHash($stream)).Replace('-','').ToLowerInvariant() -cne $entry[2]) { throw 'Catalog tool closure byte identity mismatch.' }
            $Inf2CatHeld.Add($stream)
        } catch { $stream.Dispose(); throw }
        finally { $digest.Dispose() }
    }
    # Original deny-write/delete handles stay live through the foreground tool exit.
    # Any external timeout/drop remains unresolved LEAD cleanup, not child retirement.
}
function Verify-Inputs {
    foreach ($archive in $Archives) {
        $path = Join-Path (Join-Path $PackageRoot 'archives') $archive[0]
        Require-Hash $path SHA512 $archive[2]
        if ((Get-Item -LiteralPath $path).Length -ne $archive[1]) { throw 'Sealed archive length mismatch.' }
    }
    foreach ($pin in $MicrosoftPins) {
        $root = switch ($pin[0]) { 'packages' {$PackageRoot} 'vc' {$VcRoot} 'wdk' {$WdkRoot} default {throw 'Invalid internal role.'} }
        Require-Hash (Join-Path $root $pin[1]) SHA256 $pin[2]
    }
    foreach ($entry in $SourcePins.GetEnumerator()) { Require-Hash (Join-Path $DriverRoot $entry.Key) SHA256 $entry.Value }
    Require-Hash $InfVerif SHA256 $InfVerifSha256
    if ((Get-Item -LiteralPath $InfVerif).Length -ne 637296) { throw 'Static InfVerif tool length mismatch.' }
    Require-File $Msbuild
    if ((Get-Item -LiteralPath $Msbuild).VersionInfo.FileVersion -ne '17.14.40.60911') { throw 'MSBuild baseline version mismatch.' }
    Require-File (Join-Path $ToolsRoot 'bin\Hostx64\x64\cl.exe')
    Require-File (Join-Path $ToolsRoot 'bin\Hostx64\x64\link.exe')
    Require-File (Join-Path $WdkRoot 'CodeAnalysis\DriverRecommendedRules.ruleset')
    foreach ($relative in @('um\x64\d3d11.lib','um\x64\dxgi.lib','um\x64\ole32.lib','um\x64\OneCoreUAP.lib','um\x64\ntdll.lib','ucrt\x64\ucrt.lib')) { Require-File (Join-Path $SdkLibRoot $relative) }
    foreach ($relative in @('lib\wdf\umdf\x64\2.25\WdfDriverStubUm.lib','lib\10.0.26100.0\um\x64\iddcx\1.10\IddCxStub.lib')) { Require-File (Join-Path $WdkRoot $relative) }
}
Verify-Inputs
if ($Mode -eq 'preflight') {
    Write-Output 'PREFLIGHT fixed archives/selected Microsoft imports+headers/shims/source bytes and tools verified. No MSBuild, compiler or driver ran; first build remains LEAD-only after ROOT script review.'
    return
}
if (-not $NativeSourceReviewed -or $NativePins.Count -ne $RequiredNativeRoles.Count) { throw 'UNSUPPORTED: ROOT has not released the current native source checkpoint; build was not started.' }
foreach ($role in $RequiredNativeRoles) {
    if (-not $NativePins.Contains($role)) { throw 'Incomplete sealed native role table.' }
    Require-Hash (Join-Path $DriverRoot $role) SHA256 $NativePins[$role]
}
# Refuse an existing output ancestor junction before creating any attempt directory.
$ancestor = $RepoRoot
foreach ($part in @('target','wp-notes','windows-idd-build')) {
    if ((Get-Item -LiteralPath $ancestor -Force).Attributes -band [IO.FileAttributes]::ReparsePoint) { throw 'Output ancestor must not be a reparse point.' }
    $next = Join-Path $ancestor $part
    if (-not [IO.Directory]::Exists($next)) { [void][IO.Directory]::CreateDirectory($next) }
    $ancestor = $next
}
if ((Get-Item -LiteralPath $ancestor -Force).Attributes -band [IO.FileAttributes]::ReparsePoint) { throw 'Output root must not be a reparse point.' }
$OutputBase = $ancestor
$Attempt = Join-Path $OutputBase ([Guid]::NewGuid().ToString('N'))
if ([IO.Directory]::Exists($Attempt) -or [IO.File]::Exists($Attempt)) { throw 'Output attempt must be fresh.' }
[void][IO.Directory]::CreateDirectory($Attempt)
foreach ($name in @('bin','obj','package','temp','msbuild-user')) { [void][IO.Directory]::CreateDirectory((Join-Path $Attempt $name)) }
$BuildRoot = $Attempt + '\'
$Properties = @(
    'Configuration=Release','Platform=x64','PlatformToolset=WindowsUserModeDriver10.0',
    'VCToolsVersion=14.44.35207','PreferredToolArchitecture=x64',
    'SignMode=Off','EnableTestSign=false','GenerateTestCertificate=false','EnableInf2cat=false',
    'Driver_SpectreMitigation=false','SpectreMitigation=false',
    'UseEnv=false','UseNativeEnvironment=false','InstallBuildTools=false','EnableVcpkgArtifactsIntegration=false',
    'ImportDirectoryBuildProps=false','ImportDirectoryBuildTargets=false',
    ('MSBuildUserExtensionsPath=' + (Join-Path $Attempt 'msbuild-user')),
    'CrosspaneNativeSourceReviewed=true',('CrosspaneBuildRoot=' + $BuildRoot)
)
# Only this script's process environment changes; no persistent settings or values are logged.
$ambientEnvironment = [Environment]::GetEnvironmentVariables('Process')
$childEnvironment = @{}
foreach ($name in @('OS','SystemRoot','WINDIR','SystemDrive','ComSpec','ProgramFiles','ProgramFiles(x86)','ProgramW6432','PROCESSOR_ARCHITECTURE','NUMBER_OF_PROCESSORS','USERPROFILE','LOCALAPPDATA','APPDATA')) {
    if ($ambientEnvironment.Contains($name)) { $childEnvironment[$name] = $ambientEnvironment[$name] }
}
foreach ($name in $ambientEnvironment.Keys) { [Environment]::SetEnvironmentVariable([string]$name, $null, 'Process') }
foreach ($name in $childEnvironment.Keys) { [Environment]::SetEnvironmentVariable([string]$name, [string]$childEnvironment[$name], 'Process') }
[Environment]::SetEnvironmentVariable('PATH', ($ToolsRoot + '\bin\Hostx64\x64;' + $SdkRoot + '\bin\10.0.26100.0\x64;' + $childEnvironment['SystemRoot'] + '\System32;' + $childEnvironment['SystemRoot']), 'Process')
[Environment]::SetEnvironmentVariable('TEMP', (Join-Path $Attempt 'temp'), 'Process')
[Environment]::SetEnvironmentVariable('TMP', (Join-Path $Attempt 'temp'), 'Process')
[Environment]::SetEnvironmentVariable('DBUS_SESSION_BUS_ADDRESS', 'unix:path=/nonexistent/crosspane-test-bus', 'Process')
[Environment]::SetEnvironmentVariable('DBUS_SYSTEM_BUS_ADDRESS', 'unix:path=/nonexistent/crosspane-test-bus', 'Process')
[Environment]::SetEnvironmentVariable('CROSSPANE_NO_MULTICAST', '1', 'Process')
$ambientEnvironment = $null; $childEnvironment = $null
Write-Output ('OWN_BUILD_ATTEMPT ' + $Attempt)
function Invoke-FixedMsbuild([string]$Phase, [string[]]$Extra) {
    $fixedArguments = @($Project, '/nologo', '/noAutoResponse', '/m:2', '/nodeReuse:false', '/verbosity:quiet', ('/p:' + ($Properties -join ';'))) + $Extra
    if ($Phase -eq 'build') {
        # LEAD167d8fa1: fixed owned UTF-8 normal logger exposes tool diagnostics.
        $fixedArguments += ('/flp:LogFile=' + (Join-Path $Attempt 'build.log') + ';Verbosity=normal;Encoding=UTF-8')
    }
    $stdoutPath = Join-Path $Attempt ($Phase + '-stdout.txt')
    $stderrPath = Join-Path $Attempt ($Phase + '-stderr.txt')
    # Exact owned project/output command line is recorded before starting. It is
    # evidence for LEAD's timeout/SSH-loss resolution, never worker cleanup authority.
    @{phase=$Phase;exe=$Msbuild;arguments=$fixedArguments;working_directory=$DriverRoot;output_root=$Attempt} | ConvertTo-Json -Depth 4 | Set-Content -LiteralPath (Join-Path $Attempt ($Phase + '-command.json')) -Encoding UTF8
    $savedErrorPreference = $ErrorActionPreference
    Push-Location -LiteralPath $DriverRoot
    try {
        # Windows PowerShell may surface native stderr as error records. Preserve it
        # while letting the foreground native invocation return its genuine exit.
        $ErrorActionPreference = 'Continue'
        $global:LASTEXITCODE = $null
        & $Msbuild @fixedArguments 1> $stdoutPath 2> $stderrPath
        $actualExit = $global:LASTEXITCODE
    } finally { $ErrorActionPreference = $savedErrorPreference; Pop-Location }
    if ($null -eq $actualExit) { throw 'MSBuild returned no native exit receipt; unresolved STOP, cleanup LEAD-only.' }
    @{phase=$Phase;state='exited';exit=$actualExit} | ConvertTo-Json | Set-Content -LiteralPath (Join-Path $Attempt ($Phase + '-exit.json')) -Encoding UTF8
    if ($actualExit -ne 0) {
        Write-Output ('MSBuild failed with actual exit ' + $actualExit + '; exact output retained, no retry or fallback.')
        exit $actualExit
    }
    $script:LastMsbuildExit = $actualExit
    return [IO.File]::ReadAllText($stdoutPath)
}
$Expected = [ordered]@{
    Configuration='Release';Platform='x64';PlatformToolset='WindowsUserModeDriver10.0';IsUserModeToolset='true';
    VCToolsVersion='14.44.35207';VCTargetsPath=($VcRoot + '\');VCToolsInstallDir=($ToolsRoot + '\');
    WindowsTargetPlatformVersion='10.0.26100.0';TargetPlatformVersion='10.0.26100.0';WDKBuildFolder='10.0.26100.0';
    WDKContentRoot=($WdkRoot + '\');WDK_NuGet='true';TargetPlatformSdkRootOverride=$SdkRoot;winsdk_cpp_x64_root=$SdkLibRoot;
    UMDF_INC_PATH=($WdkRoot + '\Include\wdf\umdf\');UMDF_LIB_PATH=($WdkRoot + '\lib\wdf\umdf\x64\');
    WDK_UM_INC_PATH=($WdkRoot + '\Include\10.0.26100.0\um\');WDK_UM_LIB_PATH=($WdkRoot + '\lib\10.0.26100.0\um\x64\');
    UmdfVersion='2.25';UMDF_VERSION_MAJOR='2';UMDF_VERSION_MINOR='25';UMDF_MINIMUM_VERSION_REQUIRED='25';
    IDDCX_VERSION_MAJOR='1';IDDCX_VERSION_MINOR='10';IDDCX_MINIMUM_VERSION_REQUIRED='10';IndirectDisplayDriver='true';
    SignMode='Off';EnableTestSign='false';GenerateTestCertificate='false';EnableInf2cat='false';Driver_SpectreMitigation='false';SpectreMitigation='false';
    OutDir=($BuildRoot + 'bin\');IntDir=($BuildRoot + 'obj\');PackageDir=($BuildRoot + 'package\');
    VcpkgManifestDirectory='';EnableVcpkgArtifactsIntegration='false';InstallBuildTools='false';UseEnv='false';UseNativeEnvironment='false';CrosspaneNativeSourceReviewed='true';RunCodeAnalysis='true';
    CodeAnalysisVSInstallDir=($VsRoot + '\');CodeAnalysisRuleSet=($WdkRoot + '\CodeAnalysis\DriverRecommendedRules.ruleset')
}
# Verify full compiler search paths, not just package roots. No ambient fallback.
$Expected['IncludePath'] = (@(($DriverRoot + '\include'),($DriverRoot + '\src'),($ToolsRoot + '\include'),($SdkRoot + '\Include\10.0.26100.0\ucrt'),($SdkRoot + '\Include\10.0.26100.0\shared'),($SdkRoot + '\Include\10.0.26100.0\um'),($WdkRoot + '\Include\10.0.26100.0\shared'),($WdkRoot + '\Include\10.0.26100.0\um'),($WdkRoot + '\Include\wdf\umdf\2.25'),($WdkRoot + '\Include\10.0.26100.0\um\iddcx\1.10')) -join ';')
$Expected['LibraryPath'] = (@(($ToolsRoot + '\lib\onecore\x64'),($ToolsRoot + '\lib\x64'),($SdkLibRoot + '\um\x64'),($SdkLibRoot + '\ucrt\x64'),($WdkRoot + '\lib\10.0.26100.0\um\x64'),($WdkRoot + '\lib\wdf\umdf\x64\2.25'),($WdkRoot + '\lib\10.0.26100.0\um\x64\iddcx\1.10')) -join ';')
# MSBuild17.14 getProperty WITHOUT targets evaluates properties only; it does not run tasks.
$raw = Invoke-FixedMsbuild 'properties' @('/getProperty:' + ($Expected.Keys -join ','))
$evaluated = $raw | ConvertFrom-Json
foreach ($entry in $Expected.GetEnumerator()) {
    $property = $evaluated.Properties.PSObject.Properties[$entry.Key]
    if ($null -eq $property -or [string]$property.Value -ine [string]$entry.Value) { throw ('An effective frozen property drifted; compiler was not started: ' + $entry.Key) }
}
# d474f300 accepts original Microsoft build-tool telemetry under existing VS settings.
# It is not suppressed, and no system/network setting is changed. No restore/download.
[void](Invoke-FixedMsbuild 'build' @('/t:Build'))
# LEAD20f53cc8: /analyze:quiet may hide diagnostics from MSBuild /WX.
# Parse only the five fixed owned XML outputs, with external entities disabled.
$analysisEvidence = @()
$analysisDefects = 0
foreach ($unit in @('Driver','Control','Monitor','Lease','Edid')) {
    $relative = 'obj\' + $unit + '.nativecodeanalysis.xml'
    $path = Join-Path $Attempt $relative
    Require-File $path
    if ((Get-Item -LiteralPath $path).Length -gt 1048576) { throw 'Owned analysis XML exceeds the static gate budget.' }
    $settings = New-Object System.Xml.XmlReaderSettings
    $settings.DtdProcessing = [System.Xml.DtdProcessing]::Prohibit
    $settings.XmlResolver = $null
    $settings.MaxCharactersInDocument = 1048576
    $reader = [System.Xml.XmlReader]::Create($path, $settings)
    $document = New-Object System.Xml.XmlDocument
    $document.XmlResolver = $null
    try { $document.Load($reader) } finally { $reader.Dispose() }
    if ($null -eq $document.DocumentElement -or $document.DocumentElement.Name -cne 'DEFECTS' -or $document.DocumentElement.NamespaceURI -cne '') { throw 'Unknown owned analysis XML root; no clean-analysis inference.' }
    foreach ($element in $document.DocumentElement.ChildNodes) {
        if ($element.NodeType -eq [System.Xml.XmlNodeType]::Element) {
            if ($element.Name -cne 'DEFECT' -or $element.NamespaceURI -cne '') { throw 'Unknown owned analysis XML finding element or namespace.' }
        } elseif ($element.NodeType -eq [System.Xml.XmlNodeType]::Whitespace -or $element.NodeType -eq [System.Xml.XmlNodeType]::SignificantWhitespace) {
            continue
        } elseif ($element.NodeType -eq [System.Xml.XmlNodeType]::Text -and [string]::IsNullOrWhiteSpace($element.Value)) {
            continue
        } else { throw 'Unknown owned analysis XML root content.' }
    }
    $count = $document.SelectNodes('/DEFECTS/DEFECT').Count
    $analysisDefects += $count
    $analysisEvidence += @{path=$relative;defects=$count;bytes=(Get-Item -LiteralPath $path).Length;sha256=(Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash}
}
@{defects=$analysisDefects;units=$analysisEvidence} | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath (Join-Path $Attempt 'analysis-summary.json') -Encoding UTF8
if ($analysisDefects -ne 0) { throw 'Owned native code analysis has defects; exact XML retained, no suppression.' }
# LEAD167d8fa1/0ffe2a0a: fixed /w /v reads only the built owned INF.
$builtInf = Join-Path $Attempt 'bin\CrosspaneIdd.inf'
Require-File $builtInf
if ($builtInf.Length -ge 260) { throw 'Built INF path exceeds documented InfVerif limit.' }
$infArguments = @('/w','/v',$builtInf)
$infStdout = Join-Path $Attempt 'infverif-stdout.txt'
$infStderr = Join-Path $Attempt 'infverif-stderr.txt'
@{phase='infverif';exe=$InfVerif;exe_sha256=$InfVerifSha256;arguments=$infArguments;working_directory=$DriverRoot;output_root=$Attempt} | ConvertTo-Json -Depth 4 | Set-Content -LiteralPath (Join-Path $Attempt 'infverif-command.json') -Encoding UTF8
$savedErrorPreference = $ErrorActionPreference
Push-Location -LiteralPath $DriverRoot
try {
    $ErrorActionPreference = 'Continue'
    $global:LASTEXITCODE = $null
    & $InfVerif @infArguments 1> $infStdout 2> $infStderr
    $infExit = $global:LASTEXITCODE
} finally { $ErrorActionPreference = $savedErrorPreference; Pop-Location }
if ($null -eq $infExit) { throw 'InfVerif returned no native exit receipt; unresolved STOP, cleanup LEAD-only.' }
@{phase='infverif';state='exited';exit=$infExit} | ConvertTo-Json | Set-Content -LiteralPath (Join-Path $Attempt 'infverif-exit.json') -Encoding UTF8
if ($infExit -ne 0) {
    Write-Output ('Static InfVerif failed with actual exit ' + $infExit + '; exact output retained, no retry or fallback.')
    exit $infExit
}
Verify-Inputs
foreach ($role in $RequiredNativeRoles) { Require-Hash (Join-Path $DriverRoot $role) SHA256 $NativePins[$role] }
Require-File (Join-Path $Attempt 'bin\CrosspaneIdd.dll')
$evidence = @()
foreach ($path in @('properties-command.json','properties-stdout.txt','properties-stderr.txt','properties-exit.json','build-command.json','build-stdout.txt','build-stderr.txt','build-exit.json','build.log','analysis-summary.json','obj\Driver.nativecodeanalysis.xml','obj\Control.nativecodeanalysis.xml','obj\Monitor.nativecodeanalysis.xml','obj\Lease.nativecodeanalysis.xml','obj\Edid.nativecodeanalysis.xml','infverif-command.json','infverif-stdout.txt','infverif-stderr.txt','infverif-exit.json','bin\CrosspaneIdd.dll','bin\CrosspaneIdd.inf')) {
    $file = Join-Path $Attempt $path
    $evidence += @{path=$path;bytes=(Get-Item -LiteralPath $file).Length;sha256=(Get-FileHash -LiteralPath $file -Algorithm SHA256).Hash}
}
@{mode='unsigned-static-only';sign_mode='Off';catalog_generation=$false;analysis_defects=$analysisDefects;infverif_exit=$infExit;native_execution=$false;artifacts=$evidence} | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath (Join-Path $Attempt 'build-receipt.json') -Encoding UTF8
Write-Output 'Unsigned static build and explicit InfVerif exited0; five analysis outputs have zero defects. Compiler outputs and receipts are in this checkout target/wp-notes/windows-idd-build. No catalog, driver/client execution, install, signing or load occurred.'
if ($Mode -ne 'package') { exit $script:LastMsbuildExit }
# A new dedicated directory has exactly the final static DLL/INF inputs. The project
# remains EnableInf2cat=false and SignMode=Off; no MSBuild target/property is bypassed.
$unsignedPackage = Join-Path $Attempt 'unsigned-package'
if ([IO.Directory]::Exists($unsignedPackage) -or [IO.File]::Exists($unsignedPackage)) { throw 'Unsigned package output must be fresh.' }
[void][IO.Directory]::CreateDirectory($unsignedPackage)
$packageDll = Join-Path $unsignedPackage 'CrosspaneIdd.dll'
$packageInf = Join-Path $unsignedPackage 'CrosspaneIdd.inf'
$packageCat = Join-Path $unsignedPackage 'CrosspaneIdd.cat'
[IO.File]::Copy((Join-Path $Attempt 'bin\CrosspaneIdd.dll'), $packageDll, $false)
[IO.File]::Copy($builtInf, $packageInf, $false)
Require-File $packageDll
Require-File $packageInf
if ([IO.File]::Exists($packageCat) -or [IO.Directory]::Exists($packageCat)) { throw 'Catalog output collides with existing state.' }
Require-Hash $Inf2Cat SHA256 $Inf2CatPackageSha256
if ((Get-Item -LiteralPath $Inf2Cat).Length -ne $Inf2CatPackageBytes) { throw 'Sealed catalog tool length changed.' }
$catalogArguments = @(('/driver:' + $unsignedPackage), '/os:10_GE_X64', '/verbose')
$catalogStdout = Join-Path $Attempt 'inf2cat-stdout.txt'
$catalogStderr = Join-Path $Attempt 'inf2cat-stderr.txt'
@{phase='inf2cat';exe=$Inf2Cat;exe_sha256=$Inf2CatPackageSha256;arguments=$catalogArguments;working_directory=$unsignedPackage;output_root=$Attempt;system_executor='LEAD';signing=$false} | ConvertTo-Json -Depth 4 | Set-Content -LiteralPath (Join-Path $Attempt 'inf2cat-command.json') -Encoding UTF8
$savedErrorPreference = $ErrorActionPreference
Push-Location -LiteralPath $unsignedPackage
try {
    $ErrorActionPreference = 'Continue'
    $global:LASTEXITCODE = $null
    & $Inf2Cat @catalogArguments 1> $catalogStdout 2> $catalogStderr
    $catalogExit = $global:LASTEXITCODE
} finally { $ErrorActionPreference = $savedErrorPreference; Pop-Location }
if ($null -eq $catalogExit) { throw 'Inf2Cat returned no native exit receipt; unresolved STOP, cleanup LEAD-only.' }
@{phase='inf2cat';state='exited';exit=$catalogExit} | ConvertTo-Json | Set-Content -LiteralPath (Join-Path $Attempt 'inf2cat-exit.json') -Encoding UTF8
if ($catalogExit -ne 0) { exit $catalogExit }
Require-File $packageCat
$expectedPackageNames = @('CrosspaneIdd.cat','CrosspaneIdd.dll','CrosspaneIdd.inf')
$actualPackageNames = @([IO.Directory]::GetFileSystemEntries($unsignedPackage) | ForEach-Object { [IO.Path]::GetFileName($_) } | Sort-Object)
if ($actualPackageNames.Count -ne 3 -or (@(Compare-Object $expectedPackageNames $actualPackageNames).Count -ne 0)) { throw 'Catalog tool produced unexpected owned package members; retain exact output and STOP.' }
Verify-Inputs
foreach ($role in $RequiredNativeRoles) { Require-Hash (Join-Path $DriverRoot $role) SHA256 $NativePins[$role] }
$packageEvidence = @()
foreach ($file in @($packageInf,$packageDll,$packageCat)) {
    Require-File $file
    $packageEvidence += @{name=[IO.Path]::GetFileName($file);bytes=(Get-Item -LiteralPath $file).Length;sha256=(Get-FileHash -LiteralPath $file -Algorithm SHA256).Hash}
}
@{mode='unsigned-package';static_receipt_sha256=(Get-FileHash -LiteralPath (Join-Path $Attempt 'build-receipt.json') -Algorithm SHA256).Hash;sign_mode='Off';enable_inf2cat_effective=$false;catalog_tool_exit=$catalogExit;catalog_tool_sha256=$Inf2CatPackageSha256;catalog_os='10_GE_X64';signature_policy='unsigned-only-no-sign-operation';install=$false;native_execution=$false;package=$unsignedPackage;artifacts=$packageEvidence} | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath (Join-Path $Attempt 'package-receipt.json') -Encoding UTF8
Write-Output ('PACKAGE unsigned CAT generated only after static gates; exact package ' + $unsignedPackage + '; no signing, install or driver/client execution occurred.')
exit $catalogExit
