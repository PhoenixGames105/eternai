# Builds libetern_engine.so for Android against the llama.cpp *shared* libraries
# that build-android.ps1 produced in the same build tree (Arm's mobile recipe:
# BUILD_SHARED_LIBS=ON + GGML_BACKEND_DL=ON + GGML_CPU_ALL_VARIANTS=ON).
#
# Configured standalone (not as part of the llama.cpp tree) so a llama.cpp
# upgrade never requires touching its CMake files.
cmake_minimum_required(VERSION 3.20)
project(etern_engine CXX)

set(CMAKE_CXX_STANDARD 17)
set(CMAKE_CXX_STANDARD_REQUIRED ON)

if (NOT DEFINED LLAMA_DIR)
    set(LLAMA_DIR "E:/P100-LLama.cpp/llama.cpp")
endif()
if (NOT DEFINED LLAMA_BUILD_DIR)
    message(FATAL_ERROR "LLAMA_BUILD_DIR must point at the llama.cpp build tree")
endif()

add_library(etern_engine SHARED etern_engine.cpp etern_server.cpp)

# common/ provides the jinja chat-template engine and the canonical sampler
# chain. Its headers pull in json.h (vendor/nlohmann) and threading.h
# (vendor/sheredom). The server sources additionally need the project root
# (for "common/log.h"), the server/mtmd/ui headers, and cpp-httplib.
target_include_directories(etern_engine PRIVATE
    "${LLAMA_DIR}"
    "${LLAMA_DIR}/include"
    "${LLAMA_DIR}/ggml/include"
    "${LLAMA_DIR}/common"
    "${LLAMA_DIR}/tools/server"
    "${LLAMA_DIR}/tools/mtmd"
    "${LLAMA_DIR}/tools/ui"
    "${LLAMA_DIR}/vendor/nlohmann"
    "${LLAMA_DIR}/vendor/sheredom"
    "${LLAMA_DIR}/vendor/cpp-httplib")

# With BUILD_SHARED_LIBS=ON every llama.cpp target is an imported shared lib.
# Only the direct dependencies are listed; the loader resolves the rest through
# each .so's DT_NEEDED entries at dlopen time, all from the app's
# nativeLibraryDir (which is also where ggml dlopens the CPU variants).
set(_etern_imported
    llama-server-impl
    llama-common
    llama
    ggml)
# The NDK toolchain sets CMAKE_FIND_ROOT_PATH_MODE_LIBRARY=ONLY, which re-roots
# find_library into the sysroot and hides the build tree. Glob directly instead.
foreach (_t ${_etern_imported})
    string(TOUPPER "${_t}" _T)
    string(REPLACE "-" "_" _T "${_T}")
    file(GLOB _found LIST_DIRECTORIES false "${LLAMA_BUILD_DIR}/bin/lib${_t}.so")
    list(GET _found 0 ${_T}_SO)
    if (NOT ${_T}_SO)
        message(FATAL_ERROR "missing shared lib for ${_t} under ${LLAMA_BUILD_DIR}/bin - run build-android.ps1 first")
    endif()
endforeach()

find_library(ANDROID_LOG_LIB log)
find_library(ANDROID_LIB android)

# dl is required because with GGML_BACKEND_DL the CPU backend is dlopen'd at
# runtime; atomic covers std::atomic on older NDK toolchains.
target_link_libraries(etern_engine
    ${LLAMA_SERVER_IMPL_SO}
    ${LLAMA_COMMON_SO}
    ${LLAMA_SO}
    ${GGML_SO}
    ${ANDROID_LOG_LIB}
    ${ANDROID_LIB}
    dl
    atomic)

target_compile_options(etern_engine PRIVATE -O2 -fvisibility=hidden)
target_link_options(etern_engine PRIVATE "-Wl,--build-id=sha1")
