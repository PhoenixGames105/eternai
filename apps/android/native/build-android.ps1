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
    # OpenCL backend. Chosen because the Adreno 740 driver rejects ggml-vulkan's
    # matmul_quant_f32 module outright (reproduced across 15 configurations and
    # with pristine upstream shaders), while its OpenCL 3.0 driver compiles and
    # runs kernels correctly. The NDK ships neither OpenCL headers nor a
    # libOpenCL.so import library, so both are supplied here: Khronos headers,
    # and the device's own loader as the link stub. That stub's SONAME is
    # libOpenCL.so, so the resulting DT_NEEDED resolves at runtime to the
    # vendor library, which /vendor/etc/public.libraries.txt allows apps to use.
    [switch]$OpenCL,
    [string]$OpenCLHeaders = "E:\eternAI\opencl-headers",
    [string]$OpenCLStub = "E:\eternAI\opencl-droid\libOpenCL.so",
    # The Vulkan SDK supplies the modern glslc (the NDK's is from 2022 and
    # cannot target vulkan1.3), the SPIRV-Headers CMake config, and the
    # FindVulkan support files. Install: winget install KhronosGroup.VulkanSDK
    [string]$VulkanSdk = "C:\VulkanSDK\1.4.363.0"
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
} elseif ($OpenCL) {
    $BuildDir = Join-Path $Native "build\opencl-$Abi"
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
    $sdkGlslc = Join-Path $VulkanSdk "Bin\glslc.exe"
    if (-not (Test-Path $sdkGlslc)) { throw "glslc not found at $sdkGlslc (install the Vulkan SDK: winget install KhronosGroup.VulkanSDK)" }
    $sysroot = "$Ndk\toolchains\llvm\prebuilt\windows-x86_64\sysroot"
    # The NDK ships libvulkan.so only per API level (it is a dlsym stub; the
    # real loader is the device's), so point at the one matching $Api.
    $vulkanStub = "$sysroot\usr\lib\aarch64-linux-android\$Api\libvulkan.so"
    if (-not (Test-Path $vulkanStub)) { throw "libvulkan.so stub not found at $vulkanStub" }
    $backend = @(
        "-DGGML_VULKAN=ON",
        # CMAKE_PREFIX_PATH lets find_package(Vulkan COMPONENTS glslc) resolve
        # from the SDK. SPIRV-Headers is resolved via the local shim instead:
        # the SDK's own config is generated for an install layout it does not
        # actually have (headers at Include\spirv, not next to the config).
        "-DCMAKE_PREFIX_PATH=$VulkanSdk",
        "-DSPIRV-Headers_DIR=$Native\cmake",
        "-DVULKAN_SDK_ROOT=$VulkanSdk",
        "-DVulkan_GLSLC_EXECUTABLE=$sdkGlslc",
        "-DVulkan_LIBRARY=$vulkanStub",
        # The SDK's Include has both the C headers (vulkan.h) and the C++
        # bindings (vulkan.hpp) that ggml-vulkan needs. The NDK sysroot only
        # has the C header, so it cannot serve as the include dir.
        "-DVulkan_INCLUDE_DIR=$VulkanSdk\Include",
        # The shader generator sub-build must compile a Windows host tool, so
        # it needs a real host compiler (MSVC), not the NDK clang. This
        # variable makes ggml-vulkan use our toolchain file verbatim instead
        # of generating one that hardcodes the NDK clang.
        "-DGGML_VULKAN_SHADERS_GEN_TOOLCHAIN=$Native\cmake\host-msvc.cmake"
    )
} elseif ($OpenCL) {
    if (-not (Test-Path (Join-Path $OpenCLHeaders "CL\cl.h"))) {
        throw "OpenCL headers not found at $OpenCLHeaders\CL\cl.h"
    }
    if (-not (Test-Path $OpenCLStub)) {
        throw "OpenCL import library not found at $OpenCLStub"
    }
    $backend = @(
        "-DGGML_OPENCL=ON",
        # FindOpenCL only needs a header root and something linkable. The NDK
        # sysroot has neither, so hand it the Khronos headers and the device's
        # own libOpenCL.so (pulled off the phone) as the link stub. Its SONAME
        # is libOpenCL.so, so the DT_NEEDED entry resolves at runtime to the
        # vendor loader rather than to a bundled copy.
        "-DOpenCL_INCLUDE_DIR=$OpenCLHeaders",
        "-DOpenCL_LIBRARY=$OpenCLStub",
        # Kernels are embedded as C string literals, so nothing has to be read
        # from disk at runtime -- there is no assets path inside the APK.
        "-DGGML_OPENCL_EMBED_KERNELS=ON",
        "-DGGML_OPENCL_USE_ADRENO_KERNELS=ON",
        # The device driver reports OpenCL 3.0.
        "-DGGML_OPENCL_TARGET_VERSION=300"
    )
}

Write-Host "==> configure ($Abi, vulkan=$Vulkan, opencl=$OpenCL)"
& $Cmake -S $LlamaDir -B $BuildDir -G Ninja @common @backend
if ($LASTEXITCODE -ne 0) { throw "configure failed" }

Write-Host "==> build llama.cpp shared libraries (server included)"
# With BUILD_SHARED_LIBS=ON every target is a .so. llama-server-impl transitively
# covers server-context, ui, httplib, mtmd, common and llama; ggml covers the
# base plus every dlopen-able CPU variant.
# The Vulkan shader generator is an ExternalProject that re-runs CMake from
# scratch; it does not inherit our -DCMAKE_MAKE_PROGRAM, so it must find ninja
# on PATH. Prepend the SDK's cmake bin dir for the duration of the build.
$env:PATH = (Split-Path $Ninja) + ";" + $env:PATH
if ($Vulkan) {
    # The shader generator sub-build compiles a Windows host tool with MSVC,
    # so the build must run inside a vcvars64 environment (cl on PATH).
    $VcVars = "C:\Program Files\Microsoft Visual Studio\2022\Community\VC\Auxiliary\Build\vcvars64.bat"
    if (-not (Test-Path $VcVars)) { throw "vcvars64.bat not found at $VcVars (Vulkan build needs the MSVC host toolchain)" }
    $buildCmd = @"
call "$VcVars" >nul && "$Cmake" --build "$BuildDir" --target llama-server-impl llama-common llama ggml -j 8
"@
    & $env:ComSpec /c $buildCmd
} else {
    & $Cmake --build $BuildDir --target llama-server-impl llama-common llama ggml -j 8
}
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
# ggml-vulkan is the Vulkan backend (a separate .so when GGML_VULKAN=ON); it
# must ship so the server can dlopen it. The CPU variants are dlopen'd the same
# way, which is why every produced .so is copied, not just the linked ones.
$SoPattern = '^(lib)?(ggml|ggml-base|ggml-cpu-[a-z0-9._]+|ggml-vulkan|ggml-opencl|llama|llama-common|llama-common-base|llama-server-impl|server-context|llama-ui|mtmd|cpp-httplib|vendor-hash)\.so$'

if (Test-Path $Socopy) {
    New-Item -ItemType Directory -Force -Path $Socopy | Out-Null
}

$produced = Get-ChildItem -Recurse $BuildDir -Filter "*.so" |
    Where-Object { $_.Name -match $SoPattern }
if (-not $produced) { throw "no llama.cpp shared libraries found under $BuildDir" }

# Only delete what this build actually replaces. A blanket "*.so" wipe made a
# -OpenCL build silently destroy libggml-vulkan.so (and vice versa), so the app
# could only ever expose one GPU backend at a time and a stored preference could
# point at a library that was no longer packaged.
$produced | ForEach-Object {
    Remove-Item (Join-Path $Socopy $_.Name) -Force -ErrorAction SilentlyContinue
}
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
