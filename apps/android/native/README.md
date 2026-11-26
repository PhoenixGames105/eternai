# Android native build prerequisites

Gradle never compiles native code. `:app:assembleDebug` only packages whatever `.so`
files already sit in `app/src/main/jniLibs/<abi>`, so run `build-android.ps1` after
editing the JNI sources or the APK ships a stale library.

```powershell
.\build-android.ps1              # CPU backend only
.\build-android.ps1 -OpenCL      # CPU + OpenCL backend
.\build-android.ps1 -Vulkan      # CPU + Vulkan backend
```

Each run configures its own build directory and copies only the libraries it produced
into `jniLibs`, so the backends accumulate instead of overwriting one another. The
picker in the app hides any backend whose `libggml-<id>.so` is not packaged.

## OpenCL

The NDK ships neither the Khronos OpenCL headers nor an import library for the driver,
so both are supplied out-of-tree. `build-android.ps1` expects them at `E:\eternAI\opencl-headers`
and `E:\eternAI\opencl-droid` (override with `-OpenCLHeaders` / `-OpenCLStub`). Neither
is committed.

- `opencl-headers/CL/*.h` — from <https://github.com/KhronosGroup/OpenCL-Headers>,
  the public `v2025` tags are fine.
- `opencl-droid/libOpenCL.so` — an import library whose SONAME is `libOpenCL.so`, so the
  `DT_NEEDED` entry resolves at runtime to the vendor driver in `/vendor/lib64`. The
  NDK deliberately does not provide one. Extract a usable stub from a device:

  ```powershell
  adb pull /vendor/lib64/libOpenCL.so E:\eternAI\opencl-droid\libOpenCL.so
  ```

  A device-pulled `.so` is vendor firmware, so it must not be committed. Anyone
  rebuilding the OpenCL backend needs a device (or the stub from the Khronos ICD
  loader) to link against.

## Vulkan

Needs the Vulkan SDK (`winget install KhronosGroup.VulkanSDK`) for a current `glslc` —
the NDK's is from 2022 and cannot compile the modern shaders. `cmake/host-msvc.cmake`
and `cmake/SPIRV-HeadersConfig.cmake` (both committed) let the shader generator's
`ExternalProject` use our toolchain verbatim and find SPIRV-Headers without a registry
entry.

As of the current Adreno driver the Vulkan backend does not work on the SM-F946U1 —
its shaders are rejected at compile time. It is kept buildable for other hardware and
for future drivers.
