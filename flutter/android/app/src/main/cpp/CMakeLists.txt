cmake_minimum_required(VERSION 3.4.1)

# 真零拷贝纹理原生库（导出 rdcore_texture_submit + JNI 助手）。
add_library(rdcore_texture SHARED rdcore_texture.cpp)

find_library(log-lib log)
target_link_libraries(rdcore_texture ${log-lib})
