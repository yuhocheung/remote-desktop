#!/usr/bin/env node
/**
 * Node 侧对拍（M0 交付物）：加载 wasm-bindgen 生成的 WASM 绑定，
 * 用与 Rust 测试相同的固定输入重算，并与 web/testvectors/*.json 的期望逐字节比对。
 *
 * 用法：node parity.mjs
 * 前置：cargo test -p rdcore-web（生成 fixture）+
 *       cargo build --release --target wasm32-unknown-unknown -p rdcore-web +
 *       wasm-bindgen --target web --out-dir web/rdcore-web/pkg \
 *         target/wasm32-unknown-unknown/release/rdcore_web.wasm
 */
import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const dir = dirname(fileURLToPath(import.meta.url));
const pkgDir = join(dir, '..', 'rdcore-web', 'pkg');

// Windows 下动态 import 必须是 file:// URL（裸绝对路径会被当成协议）。
const { initSync, ...rdweb } = await import(pathToFileURL(join(pkgDir, 'rdcore_web.js')).href);
const wasmBytes = readFileSync(join(pkgDir, 'rdcore_web_bg.wasm'));
initSync({ module: wasmBytes });

const load = (name) => JSON.parse(readFileSync(join(dir, name), 'utf8'));
const identity = load('identity.json');
const messages = load('messages.json');
const crypto = load('crypto.json');
const fragment = load('fragment.json');

let passed = 0;
let failed = 0;
function check(name, actual, expected) {
  const a = typeof actual === 'string' ? actual : JSON.stringify(actual);
  const e = typeof expected === 'string' ? expected : JSON.stringify(expected);
  if (a === e) {
    passed++;
    console.log(`  ok   ${name}`);
  } else {
    failed++;
    console.log(`  FAIL ${name}\n       expected: ${e.slice(0, 160)}\n       actual:   ${a.slice(0, 160)}`);
  }
}
function hex(u8) {
  return [...u8].map((b) => b.toString(16).padStart(2, '0')).join('');
}
function unhex(s) {
  const out = new Uint8Array(s.length / 2);
  for (let i = 0; i < out.length; i++) out[i] = parseInt(s.slice(i * 2, i * 2 + 2), 16);
  return out;
}

console.log('== ① 身份（固定种子 → 公钥 / 指纹）==');
{
  const pub = JSON.parse(
    rdweb.identity_from_seed(identity.viewer.seed_hex, identity.viewer.device_id_hex, 'web-viewer-fixture'),
  );
  check('viewer device_id', pub.device_id_hex, identity.viewer.device_id_hex);
  check('viewer public_key', pub.public_key_hex, identity.viewer.public_key_hex);
  check('viewer fingerprint', pub.fingerprint, identity.viewer.fingerprint);
  check('identity_public 一致', JSON.parse(rdweb.identity_public()).public_key_hex, identity.viewer.public_key_hex);
}

console.log('== ② 身份导出 / 导入（KDF + XChaCha，固定 blob 与随机往返）==');
{
  // 固定盐/nonce 的黄金 blob 必须能导入并还原同一身份。
  const pub = JSON.parse(rdweb.identity_import(unhex(identity.export.blob_hex), identity.export.passphrase));
  check('固定 blob 导入还原公钥', pub.public_key_hex, identity.export.expect_public_key_hex);
  // 随机盐/nonce 的导出 → 导入往返。
  const blob = rdweb.identity_export(identity.export.passphrase);
  const back = JSON.parse(rdweb.identity_import(blob, identity.export.passphrase));
  check('导出/导入往返', back.public_key_hex, identity.viewer.public_key_hex);
  // 错误口令必须抛错。
  let threw = false;
  try {
    rdweb.identity_import(blob, 'wrong-pass');
  } catch {
    threw = true;
  }
  check('错误口令拒绝', threw, true);
}

console.log('== ③ 握手：PeerHello / 签名 Offer / Answer / ICE（postcard 字节级一致）==');
{
  const hs = new rdweb.WebHandshake(messages.session_id_hex);
  check('peer_hello', hex(hs.peer_hello()), messages.peer_hello_viewer_hex);

  // 通用解析：Host 的 PeerHello（TOFU 记住，后续验签的前提）。
  const helloInfo = JSON.parse(hs.handle_message(unhex(messages.peer_hello_host_hex)));
  check('handle_message(peer_hello).kind', helloInfo.kind, 'peer_hello');
  check('peer_hello.public_key', helloInfo.public_key_hex, identity.host.public_key_hex);

  const offer = hs.build_signed_offer(messages.sdp_offer, messages.capabilities_json);
  check('签名 Offer 字节', hex(offer), messages.offer_signed_hex);
  check('handle_message(offer).kind', JSON.parse(hs.handle_message(offer)).kind, 'offer');

  const answerInfo = JSON.parse(hs.handle_answer(unhex(messages.answer_signed_hex)));
  check('answer.sdp', answerInfo.sdp, messages.sdp_answer);
  check('answer.from', answerInfo.from_device_id_hex, identity.host.device_id_hex);
  check('answer.fingerprint', answerInfo.fingerprint, identity.host.fingerprint);

  const ice = hs.build_ice(messages.ice_candidate_json, '0', 0);
  check('ICE 字节', hex(ice), messages.ice_hex);
  const iceInfo = JSON.parse(hs.handle_message(ice));
  check('handle_message(ice).candidate', iceInfo.candidate, messages.ice_candidate_json);

  console.log('== ④ 会话密钥交换（确定性临时密钥）==');
  const ske = hs.build_session_key_exchange_det(crypto.x25519.viewer_secret_hex);
  check('Viewer SessionKeyExchange 字节', hex(ske), messages.session_key_exchange_viewer_hex);
  hs.handle_session_key_exchange(unhex(messages.session_key_exchange_host_hex));
  check('会话密钥已建立', hs.has_session_key(), true);
  check('ECDH 会话密钥', hex(hs.session_key_bytes()), crypto.x25519.session_key_hex);

  console.log('== ⑤ 分片重组 + 媒体帧解密 + 控制消息加解密 ==');
  const media = new rdweb.FramePipeline();
  media.set_session_key(hs.session_key_bytes());
  check('pipeline 会话密钥已设置', media.has_session_key(), true);

  // 黄金媒体帧：postcard(MediaFrame{data: postcard(Ciphertext)}) → 解密还原像素。
  const mf = JSON.parse(media.decrypt_media_frame(unhex(crypto.media_frame.payload_hex)));
  check('媒体帧 codec', mf.codec, crypto.media_frame.codec);
  check('媒体帧 width', mf.width, crypto.media_frame.width);
  check('媒体帧 height', mf.height, crypto.media_frame.height);
  check('媒体帧像素', mf.data_hex, crypto.media_frame.pixels_hex);

  // 黄金密文控制消息：postcard(Message::Encrypted) → 解密为 AppMessage 明文。
  const opened = media.decrypt_control_message(unhex(crypto.aead.encrypted_message_hex));
  check('控制消息解密', hex(opened), crypto.aead.plaintext_hex);
  const appJson = JSON.parse(rdweb.app_message_to_json(opened));
  check('AppMessage.kind', appJson.kind, 'input');

  // 加密 → 解密往返（随机 nonce，只验往返）。
  const sealed = media.encrypt_control_message(unhex(messages.app_input_mouse_move_hex));
  const back = media.decrypt_control_message(sealed);
  check('控制消息加解密往返', hex(back), messages.app_input_mouse_move_hex);

  // 发送侧构造：输入事件字节必须与 fixture 一致。
  const mv = rdweb.build_input_mouse_move(1n, 640, 360);
  check('build_input_mouse_move', hex(mv), messages.app_input_mouse_move_hex);

  console.log('== ⑥ SCTP 分片（整包 / 2 片 / 多片 / 全链路重组）==');
  for (const c of fragment.cases) {
    // 发送侧：frame_wrap + 分片，字节必须与 fixture 一致。
    const framed = unhex(c.framed_hex);
    const count = rdweb.sctp_chunk_count(framed.length);
    check(`${c.name} 片数`, count, c.chunks.length);
    let ok = true;
    for (let i = 0; i < count; i++) {
      if (hex(rdweb.sctp_chunk(framed, i)) !== c.chunks[i]) ok = false;
    }
    check(`${c.name} 分片字节`, ok, true);
    // 接收侧：逐片喂入，最后一片应吐出完整 postcard 负载。
    const p = new rdweb.FramePipeline();
    let out;
    for (const chunkHex of c.chunks) {
      const r = p.push_sctp_message(unhex(chunkHex));
      if (r !== undefined && r !== null) out = r;
    }
    check(`${c.name} 重组负载`, hex(out ?? new Uint8Array()), c.payload_hex);
  }
}

console.log(`\n${failed === 0 ? 'PARITY OK' : 'PARITY FAILED'}: ${passed} passed, ${failed} failed`);
process.exit(failed === 0 ? 0 : 1);
