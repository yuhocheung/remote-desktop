#!/usr/bin/env python3
"""rdcore_ffi 的二进制级 FFI 验证（无需 Flutter / Dart 引擎）。

直接用 ctypes 加载已编译的 rdcore_ffi.dll，跑完「带外配对 → Offer/Answer 验签 →
同意门控 → 端到端 X25519+ECDH 密钥交换 → AEAD 加解密」全链路，断言每一步都成功。
这把「Flutter NativeConnection 路径是否真的能接通 Rust 核心」在二进制层面坐实，
而 Mock 后端测试覆盖不到这一点。

运行：python3 tests/ffi_ctypes_smoke.py
（默认在 target/debug/rdcore_ffi.dll 找库；可用 RDCORE_FFI_DLL 环境变量覆盖。）
"""
import ctypes
import os
import sys
from pathlib import Path

HERE = Path(__file__).resolve()
REPO_ROOT = HERE.parents[4]  # .../remote-desktop

DLL_PATH = os.environ.get(
    "RDCORE_FFI_DLL",
    str(REPO_ROOT / "target" / "debug" / "rdcore_ffi.dll"),
)

if not os.path.exists(DLL_PATH):
    print(f"SKIP: rdcore_ffi.dll 不存在于 {DLL_PATH}（先 `cargo build -p rdcore-ffi`）")
    sys.exit(0)

lib = ctypes.CDLL(DLL_PATH)


class RdBytes(ctypes.Structure):
    _fields_ = [("data", ctypes.c_void_p), ("len", ctypes.c_uint64)]


# ───────── 函数签名声明 ─────────
lib.rdcore_version.restype = ctypes.c_void_p
lib.rdcore_last_error.restype = ctypes.c_void_p
lib.rdcore_string_free.argtypes = [ctypes.c_void_p]
lib.rdcore_bytes_free.argtypes = [ctypes.c_void_p]

lib.rdcore_identity_new.restype = ctypes.c_void_p
lib.rdcore_identity_new.argtypes = [ctypes.c_void_p]
lib.rdcore_identity_free.argtypes = [ctypes.c_void_p]

lib.rdcore_local_fingerprint.restype = ctypes.c_void_p
lib.rdcore_local_fingerprint.argtypes = [ctypes.c_void_p]
lib.rdcore_local_device_id.restype = ctypes.c_void_p
lib.rdcore_local_device_id.argtypes = [ctypes.c_void_p]
lib.rdcore_local_peer_json.restype = ctypes.c_void_p
lib.rdcore_local_peer_json.argtypes = [ctypes.c_void_p]
lib.rdcore_remember_peer_json.restype = ctypes.c_void_p
lib.rdcore_remember_peer_json.argtypes = [ctypes.c_void_p, ctypes.c_void_p]

lib.rdcore_session_new.restype = ctypes.c_void_p
lib.rdcore_session_new.argtypes = [ctypes.c_void_p, ctypes.c_int, ctypes.c_void_p, ctypes.c_void_p]
lib.rdcore_session_free.argtypes = [ctypes.c_void_p]

lib.rdcore_make_offer.restype = ctypes.c_void_p
lib.rdcore_make_offer.argtypes = [ctypes.c_void_p]
lib.rdcore_ingest_offer.restype = ctypes.c_void_p
lib.rdcore_ingest_offer.argtypes = [ctypes.c_void_p, ctypes.c_void_p, ctypes.c_uint64]
lib.rdcore_make_answer.restype = ctypes.c_void_p
lib.rdcore_make_answer.argtypes = [ctypes.c_void_p]
lib.rdcore_ingest_answer.restype = ctypes.c_void_p
lib.rdcore_ingest_answer.argtypes = [ctypes.c_void_p, ctypes.c_void_p, ctypes.c_uint64]
lib.rdcore_make_session_key_exchange.restype = ctypes.c_void_p
lib.rdcore_make_session_key_exchange.argtypes = [ctypes.c_void_p]
lib.rdcore_ingest_session_key_exchange.restype = ctypes.c_void_p
lib.rdcore_ingest_session_key_exchange.argtypes = [ctypes.c_void_p, ctypes.c_void_p, ctypes.c_uint64]

lib.rdcore_encrypt.restype = ctypes.c_void_p
lib.rdcore_encrypt.argtypes = [ctypes.c_void_p, ctypes.c_void_p, ctypes.c_uint64]
lib.rdcore_decrypt.restype = ctypes.c_void_p
lib.rdcore_decrypt.argtypes = [ctypes.c_void_p, ctypes.c_void_p, ctypes.c_uint64]

lib.rdcore_host_request_consent.restype = ctypes.c_void_p
lib.rdcore_host_request_consent.argtypes = [ctypes.c_void_p, ctypes.c_void_p]
lib.rdcore_host_decide.restype = ctypes.c_void_p
lib.rdcore_host_decide.argtypes = [ctypes.c_void_p, ctypes.c_int, ctypes.c_uint32, ctypes.c_int64]
lib.rdcore_tick.restype = ctypes.c_void_p
lib.rdcore_tick.argtypes = [ctypes.c_void_p]
lib.rdcore_heartbeat.argtypes = [ctypes.c_void_p]
lib.rdcore_revoke.restype = ctypes.c_void_p
lib.rdcore_revoke.argtypes = [ctypes.c_void_p]
lib.rdcore_on_disconnected.restype = ctypes.c_void_p
lib.rdcore_on_disconnected.argtypes = [ctypes.c_void_p]
lib.rdcore_connection_state.restype = ctypes.c_void_p
lib.rdcore_connection_state.argtypes = [ctypes.c_void_p]
lib.rdcore_security_indicator.restype = ctypes.c_void_p
lib.rdcore_security_indicator.argtypes = [ctypes.c_void_p, ctypes.c_int]
lib.rdcore_peer_display_name.restype = ctypes.c_void_p
lib.rdcore_peer_display_name.argtypes = [ctypes.c_void_p]
lib.rdcore_peer_fingerprint.restype = ctypes.c_void_p
lib.rdcore_peer_fingerprint.argtypes = [ctypes.c_void_p]
lib.rdcore_peer_device_id.restype = ctypes.c_void_p
lib.rdcore_peer_device_id.argtypes = [ctypes.c_void_p]


# ───────── 安全封装 ─────────
def read_str(ptr):
    if not ptr:
        return None
    raw = ctypes.cast(ptr, ctypes.c_char_p).value
    lib.rdcore_string_free(ptr)
    return raw.decode("utf-8") if raw is not None else None


def read_bytes(ptr):
    if not ptr:
        return None
    rb = ctypes.cast(ptr, ctypes.POINTER(RdBytes))[0]
    data = ctypes.string_at(rb.data, rb.len)
    lib.rdcore_bytes_free(ptr)
    return data


def cstr(s: str) -> ctypes.c_void_p:
    buf = ctypes.create_string_buffer(s.encode("utf-8"))
    return ctypes.cast(buf, ctypes.c_void_p)


def send_bytes(fn, session, payload: bytes) -> ctypes.c_void_p:
    buf = ctypes.create_string_buffer(payload, len(payload))
    p = ctypes.cast(buf, ctypes.c_void_p)
    return fn(session, p, ctypes.c_uint64(len(payload)))


def check_cmd(err_ptr, ctx: str):
    if err_ptr:
        msg = read_str(err_ptr)
        raise AssertionError(f"{ctx} 失败: {msg}")


def make_bytes(fn, session) -> bytes:
    ptr = fn(session)
    data = read_bytes(ptr)
    assert data is not None, f"{fn.__name__} 返回 NULL"
    return data


def make_bytes1(fn, session, payload: bytes) -> bytes:
    buf = ctypes.create_string_buffer(payload, len(payload))
    p = ctypes.cast(buf, ctypes.c_void_p)
    ptr = fn(session, p, ctypes.c_uint64(len(payload)))
    data = read_bytes(ptr)
    assert data is not None, f"{fn.__name__} 返回 NULL"
    return data


# ───────── 测试 ─────────
failures = []


def check(cond, msg):
    if cond:
        print(f"  ✓ {msg}")
    else:
        print(f"  ✗ {msg}")
        failures.append(msg)


print(f"加载 {DLL_PATH}")
ver = read_str(lib.rdcore_version())
print(f"  rdcore_version = {ver}")
check(ver is not None and len(ver) > 0, "库加载成功且 rdcore_version 可读")

# 1) 两个设备的长期身份
host_local = lib.rdcore_identity_new(cstr("host-laptop"))
viewer_local = lib.rdcore_identity_new(cstr("viewer-phone"))
check(not (host_local is None or host_local == 0), "Host 身份创建成功")
check(not (viewer_local is None or viewer_local == 0), "Viewer 身份创建成功")

host_fp = read_str(lib.rdcore_local_fingerprint(host_local))
viewer_fp = read_str(lib.rdcore_local_fingerprint(viewer_local))
check(host_fp and viewer_fp and host_fp != viewer_fp, f"双方指纹不同: {host_fp} vs {viewer_fp}")

# 2) 带外配对：互相导入对方身份 JSON
host_peer_json = read_str(lib.rdcore_local_peer_json(host_local))
viewer_peer_json = read_str(lib.rdcore_local_peer_json(viewer_local))
check(host_peer_json and viewer_peer_json, "导出对端身份 JSON 成功")
err = lib.rdcore_remember_peer_json(viewer_local, cstr(host_peer_json))
check_cmd(err, "viewer 导入 host 身份")
err = lib.rdcore_remember_peer_json(host_local, cstr(viewer_peer_json))
check_cmd(err, "host 导入 viewer 身份")

# 3) 开会话
sid = bytes([1]) * 16
sidbuf = ctypes.create_string_buffer(sid, 16)
sidp = ctypes.cast(sidbuf, ctypes.c_void_p)
host = lib.rdcore_session_new(host_local, ctypes.c_int(1), sidp, ctypes.c_void_p(0))
viewer = lib.rdcore_session_new(viewer_local, ctypes.c_int(0), sidp, ctypes.c_void_p(0))
check(not (host is None or host == 0), "Host 会话创建成功")
check(not (viewer is None or viewer == 0), "Viewer 会话创建成功")

# 4) Viewer 发 Offer → Host 验签收下
offer = make_bytes(lib.rdcore_make_offer, viewer)
err = send_bytes(lib.rdcore_ingest_offer, host, offer)
check_cmd(err, "host 验签 viewer Offer")
print("  ✓ Offer 经 FFI 生成并被 Host 验签通过")

# 5) Host 回 Answer → Viewer 验签收下
answer = make_bytes(lib.rdcore_make_answer, host)
err = send_bytes(lib.rdcore_ingest_answer, viewer, answer)
check_cmd(err, "viewer 验签 host Answer")
print("  ✓ Answer 经 FFI 生成并被 Viewer 验签通过")

# 6) Host 同意（View + Input）
st = read_str(lib.rdcore_host_request_consent(host, ctypes.c_void_p(0)))
check(st is not None and "AwaitingConsent" in st, f"Host 进入同意流程（交互模式等待决定）: {st}")
st = read_str(lib.rdcore_host_decide(host, ctypes.c_int(1), ctypes.c_uint32(1 | 2), ctypes.c_int64(0)))
check(st is not None and "Active" in st, f"Host 批准 → 激活: {st}")

# 7) 端到端密钥交换
v_ex = make_bytes(lib.rdcore_make_session_key_exchange, viewer)
h_ex = make_bytes(lib.rdcore_make_session_key_exchange, host)
err = send_bytes(lib.rdcore_ingest_session_key_exchange, host, v_ex)
check_cmd(err, "host 接受 viewer 密钥交换")
err = send_bytes(lib.rdcore_ingest_session_key_exchange, viewer, h_ex)
check_cmd(err, "viewer 接受 host 密钥交换")
print("  ✓ X25519 会话密钥经 FFI 双向交换并建立")

# 8) E2E 加密：Viewer 加密 → Host 解密
plain = b"hello remote desktop \x00\x01\x02"
ct = make_bytes1(lib.rdcore_encrypt, viewer, plain)
dec = make_bytes1(lib.rdcore_decrypt, host, ct)
check(dec == plain, "E2E AEAD 加解密往返一致")

# 篡改应解密失败
ct_bad = bytearray(ct)
ct_bad[0] ^= 0xFF
bad = lib.rdcore_decrypt(host, ctypes.cast(ctypes.create_string_buffer(bytes(ct_bad), len(ct_bad)), ctypes.c_void_p), ctypes.c_uint64(len(ct_bad)))
check(bad is None or bad == 0, "篡改密文被拒绝（解密返回 NULL）")

# 9) 不可伪造安全指示器
ind = read_str(lib.rdcore_security_indicator(host, ctypes.c_int(1)))
check(ind is not None and "encrypted" in ind, f"安全指示器含加密标记: {ind}")

# 10) 已认证对端信息
pn = read_str(lib.rdcore_peer_display_name(host))
pf = read_str(lib.rdcore_peer_fingerprint(host))
check(pn == "viewer-phone", f"Host 看到对端名: {pn}")
check(pf == viewer_fp, "Host 看到对端指纹与本地一致")

# 11) 撤销
st = read_str(lib.rdcore_revoke(host))
check(st is not None and "Closed" in st, f"Host 撤销 → 关闭: {st}")

# 12) 未配对对端应被拒（安全）
a = lib.rdcore_identity_new(cstr("A"))
b = lib.rdcore_identity_new(cstr("B"))
sa = lib.rdcore_session_new(a, ctypes.c_int(1), sidp, ctypes.c_void_p(0))
sb = lib.rdcore_session_new(b, ctypes.c_int(0), sidp, ctypes.c_void_p(0))
offer_b = make_bytes(lib.rdcore_make_offer, sb)
err = send_bytes(lib.rdcore_ingest_offer, sa, offer_b)
check(err is not None and err != 0, "未配对对端的 Offer 被拒绝（MITM 防护）")
lib.rdcore_bytes_free(None)  # 空指针应安全
lib.rdcore_string_free(None)

# 清理
lib.rdcore_session_free(host)
lib.rdcore_session_free(viewer)
lib.rdcore_identity_free(host_local)
lib.rdcore_identity_free(viewer_local)
lib.rdcore_session_free(sa)
lib.rdcore_session_free(sb)
lib.rdcore_identity_free(a)
lib.rdcore_identity_free(b)

print()
if failures:
    print(f"FFI 验证 FAIL（{len(failures)} 项）:")
    for f in failures:
        print(f"  - {f}")
    sys.exit(1)
print("FFI 二进制级验证全部通过 ✓")
