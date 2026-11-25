# Cross-compiles llama.cpp for Android arm64-v8a using Arm's exact mobile recipe
# from llama.cpp's examples/llama.android (the build known to work on-device).
#
#   .\build-android.ps1              # CPU backend (default)
#   .\build-android.ps1 -Vulkan      # Vulkan backend (needs SPIRV-Headers + glslc)
#
# Gradle never compiles native code: :app:assembleDebug only packages whatever
# .so files already sit in app/src/main/jniLibs. Re-run this script after editing
# the JNI sources, or the APK ships a stale library.
#
# Arm's recipe (examples/llama.android/lib/build.gradle.kts + cpp/CMakeLists.txt):
#   BUILD_SHARED_LIBS=ON, GGML_NATIVE=OFF, GGML_BACKEND_DL=ON,
#   GGML_CPU_ALL_VARIANTS=ON, GGML_LLAMAFILE=OFF, GGML_CPU_KLEIDIAI=ON,
#   GGML_OPENMP=ON, NDK 29.0.13113456, CMake 3.31.6.
# With GGML_BACKEND_DL the CPU backend is NOT linked into libggml; instead one
# shared library per ISA variant (libggml-cpu-armv8.so, -dotprod.so, -i8mm.so,
# -sve.so, -sve2.so) is dlopen'd at runtime from the app's nativeLibraryDir.
# The APK therefore ships every produced .so and the manifest extracts native
# libs (useLegacyPackaging). Our previous single static .so with LLAMAFILE=ON
# and no KleidiAI diverged from the known-good recipe in every flag.
param(
    [string]$Ndk = "C:\Android\ndk\29.0.13113456",
    [string]$LlamaDir = "E:\P100-LLama.cpp\llama.cpp",
    [string]$Abi = "arm64-v8a",
    [int]$Api = 28,
    [switch]$Vulkan,
    [string]$SpirvHeaders = "C:\spirv-headers",
    [string]$Glslc = "$Ndk\shader-tools\windows-x86_64\glslc.exe"
)

# Native tools write warnings to stderr; with "Stop" PowerShell 5.1 promotes
# those to terminating errors, so failures are detected via $LASTEXITCODE.
$ErrorActionPreference = "Continue"
$Native = $PSScriptRoot
# Use the Android SDK's CMake, not a standalone one. CMake 4.x sets WIN32=1 even
# when cross-compiling to Android, which makes llama.cpp's vendor/cpp-httplib add
# `-lws2_32` and the link fails with "unable to find library -lws2_32". The SDK's
# 3.31.6 (the version Arm's examples/llama.android pins) leaves WIN32 empty.
$Cmake = "C:\Android\cmake\3.31.6\bin\cmake.exe"
$Ninja = "C:\Android\cmake\3.31.6\bin\ninja.exe"
$Toolchain = Join-Path $Ndk "build\cmake\android.toolchain.cmake"

foreach ($p in @($Toolchain, $Cmake, $Ninja, $LlamaDir)) {
    if (-not (Test-Path $p)) { throw "missing: $p" }
}

if ($Vulkan) {
    $BuildDir = Join-Path $Native "build\vulkan-$Abi"
} else {
    $BuildDir = Join-Path $Native "build\cpu-$Abi"
}
New-Item -ItemType Directory -Force -Path $BuildDir | Out-Null

# Arm's flag set verbatim, plus what the embedded llama-server additionally
# needs (server on, curl/ui/tests off). API 28 (not Arm's 33) because Bionic
# gained aligned_alloc there and our minSdk is 28; nothing else in the recipe
# depends on the platform level.
$common = @(
    "-DCMAKE_MAKE_PROGRAM=$Ninja",
    "-DCMAKE_TOOLCHAIN_FILE=$Toolchain",
    "-DANDROID_ABI=$Abi",
    "-DANDROID_PLATFORM=android-$Api",
    "-DCMAKE_BUILD_TYPE=Release",
    "-DBUILD_SHARED_LIBS=ON",
    "-DLLAMA_BUILD_TESTS=OFF",
    "-DLLAMA_BUILD_EXAMPLES=OFF",
    "-DLLAMA_BUILD_SERVER=ON",
    "-DLLAMA_BUILD_UI=OFF",
    "-DLLAMA_USE_PREBUILT_UI=OFF",
    "-DLLAMA_CURL=OFF",
    "-DLLAMA_OPENSSL=OFF",
    "-DGGML_NATIVE=OFF",
    "-DGGML_BACKEND_DL=ON",
    "-DGGML_CPU_ALL_VARIANTS=ON",
    "-DGGML_LLAMAFILE=OFF",
    "-DGGML_CPU_KLEIDIAI=ON",
    "-DGGML_OPENMP=ON"
)

# The embedded server is what actually serves tokens now, so the llama.cpp tree
# must be configured with the server on. A stale UI dist from an earlier probe
# (a different llama-ui asset layout) would fail ui_validate_assets, so clear it
# before configuring; the server then builds with an empty UI table, which is
# fine because the app talks to the OpenAI endpoint, not the built-in web UI.
$UiDist = Join-Path $BuildDir "tools\ui"
if (Test-Path $UiDist) {
    Get-ChildItem $UiDist -Force | Where-Object { $_.Name -in @('dist', '.ui-stamp', 'dist.tar.gz', 'dist.tar.gz.lock') } |
        ForEach-Object { Remove-Item $_.FullName -Recurse -Force -ErrorAction SilentlyContinue }
}

$backend = @("-DGGML_VULKAN=OFF")
if ($Vulkan) {
    if (-not (Test-Path $Glslc)) { throw "glslc not found at $Glslc" }
    $sysroot = "$Ndk\toolchains\llvm\prebuilt\windows-x86_64\sysroot"
    $backend = @(
        "-DGGML_VULKAN=ON",
        "-DVulkan_GLSLC_EXECUTABLE=$Glslc",
        "-DVulkan_LIBRARY=$sysroot\usr\lib\aarch64-linux-android\libvulkan.so",
        "-DVulkan_INCLUDE_DIR=$sysroot\usr\include",
        "-DSPIRV-Headers_SOURCE_DIR=$SpirvHeaders",
        "-DSPIRV_HEADERS_INCLUDE_DIR=$SpirvHeaders\include"
    )
}

Write-Host "==> configure ($Abi, vulkan=$Vulkan)"
& $Cmake -S $LlamaDir -B $BuildDir -G Ninja @common @backend
if ($LASTEXITCODE -ne 0) { throw "configure failed" }

Write-Host "==> build llama.cpp shared libraries (server included)"
# With BUILD_SHARED_LIBS=ON every target is a .so. llama-server-impl transitively
# covers server-context, ui, httplib, mtmd, common and llama; ggml covers the
# base plus every dlopen-able CPU variant.
& $Cmake --build $BuildDir --target llama-server-impl llama-common llama ggml -j 8
if ($LASTEXITCODE -ne 0) { throw "build failed" }

# ---------------------------------------------------------------- JNI wrapper
$EngineBuild = Join-Path $Native "build\engine-$Abi"
New-Item -ItemType Directory -Force -Path $EngineBuild | Out-Null

Write-Host "==> configure libetern_engine"
& $Cmake -S $Native -B $EngineBuild -G Ninja `
    "-DCMAKE_MAKE_PROGRAM=$Ninja" `
    "-DCMAKE_TOOLCHAIN_FILE=$Toolchain" `
    "-DANDROID_ABI=$Abi" `
    "-DANDROID_PLATFORM=android-$Api" `
    "-DCMAKE_BUILD_TYPE=Release" `
    "-DLLAMA_DIR=$($LlamaDir -replace '\\','/')" `
    "-DLLAMA_BUILD_DIR=$($BuildDir -replace '\\','/')"
if ($LASTEXITCODE -ne 0) { throw "engine configure failed" }

& $Cmake --build $EngineBuild --target etern_engine -j 8
if ($LASTEXITCODE -ne 0) { throw "engine build failed" }

# ------------------------------------------------------------------ packaging
# The APK must carry every llama.cpp .so: the linker resolves the DT_NEEDED
# chain inside the app's nativeLibraryDir, and ggml_backend_load_all_from_path
# dlopens the ggml-cpu-<variant>.so files from that same directory (exactly
# what Arm's example app does).
$Socopy = Join-Path $Native "..\app\src\main\jniLibs\$Abi"
# Android variant names look like ggml-cpu-android_armv8.2_2, so the character
# class needs underscores and dots as well as alphanumerics.
$SoPattern = '^(lib)?(ggml|ggml-base|ggml-cpu-[a-z0-9._]+|llama|llama-common|llama-common-base|llama-server-impl|server-context|llama-ui|mtmd|cpp-httplib|vendor-hash)\.so$'

if (Test-Path $Socopy) {
    Remove-Item (Join-Path $Socopy "*.so") -Force -ErrorAction SilentlyContinue
} else {
    New-Item -ItemType Directory -Force -Path $Socopy | Out-Null
}

$produced = Get-ChildItem -Recurse $BuildDir -Filter "*.so" |
    Where-Object { $_.Name -match $SoPattern }
if (-not $produced) { throw "no llama.cpp shared libraries found under $BuildDir" }
$produced | ForEach-Object { Copy-Item $_.FullName $Socopy -Force }

$EngineSo = Join-Path $EngineBuild "libetern_engine.so"
if (-not (Test-Path $EngineSo)) { throw "libetern_engine.so not produced" }
Copy-Item $EngineSo $Socopy -Force

# GGML_OPENMP=ON makes every CPU variant a DT_NEEDED consumer of libomp.so. The
# NDK keeps it outside the sysroot (Gradle's own externalNativeBuild copies it;
# we invoke CMake directly, so copy it by hand) and llvm-strip would otherwise
# drop its OpenMP entry points.
$Omp = Get-ChildItem (Join-Path $Ndk "toolchains\llvm\prebuilt\windows-x86_64\lib\clang") -Recurse -Filter libomp.so |
    Where-Object { $_.FullName -match '\\aarch64\\libomp\.so$' } | Select-Object -First 1
if (-not $Omp) { throw "libomp.so not found in NDK (required by GGML_OPENMP)" }
Copy-Item $Omp.FullName $Socopy -Force

# Debug info dominates the size; strip everything shipped. libomp.so is exempt:
# no library links it at build time (it is only a runtime DT_NEEDED of the CPU
# variants), so --strip-unneeded would discard entry points the loader resolves.
$Strip = Join-Path $Ndk "toolchains\llvm\prebuilt\windows-x86_64\bin\llvm-strip.exe"
if (Test-Path $Strip) {
    Get-ChildItem $Socopy -Filter *.so | Where-Object { $_.Name -ne 'libomp.so' } |
        ForEach-Object { & $Strip --strip-unneeded $_.FullName }
}
Get-ChildItem $Socopy -Filter *.so | ForEach-Object { "{0}  {1:N0} bytes" -f $_.Name, $_.Length }

# Every `external fun` in LlamaNative.kt must have a matching symbol, or the app
# fails at call time with "No implementation found" — which a green Gradle build
# cannot see. Checked by name, not by count, so a rename cannot pass by accident.
$Nm = Join-Path $Ndk "toolchains\llvm\prebuilt\windows-x86_64\bin\llvm-nm.exe"
if (Test-Path $Nm) {
    $kt = Get-Content (Join-Path $Native "..\app\src\main\java\dev\eternai\app\engine\LlamaNative.kt") -Raw
    $wanted = [regex]::Matches($kt, 'external fun\s+(\w+)') | ForEach-Object { $_.Groups[1].Value } | Sort-Object -Unique
    $have = & $Nm -D --defined-only $EngineSo | ForEach-Object { ($_ -split '\s+')[-1] }
    $missing = @($wanted | Where-Object { $have -notcontains "Java_dev_eternai_app_engine_LlamaNative_$_" })
    "JNI exports: $(@($wanted).Count) declared in Kotlin"
    if ($missing.Count -gt 0) {
        throw "libetern_engine.so is missing JNI symbols: $($missing -join ', '). Rebuild this script before the APK."
    }
    $wanted | ForEach-Object { "  Java_dev_eternai_app_engine_LlamaNative_$_" }
}
