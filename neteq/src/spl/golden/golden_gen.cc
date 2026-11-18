// Golden-value generator for the WP1 port (neteq/src/spl and the NetEq
// vectors/helpers). Builds against the generic-C WebRTC sources at rev
// 6341cca49ffcad6fcb4603bd6cd1325fd0fe28bd and prints one FNV-1a hash per test.
// The Rust tests replay the same xorshift32 input stream and compare hashes.
#include <cstdint>
#include <cstdio>
#include <cstring>
#include <vector>
#include <algorithm>

#include "common_audio/signal_processing/include/signal_processing_library.h"
#include "common_audio/signal_processing/include/spl_inl.h"
#include "common_audio/signal_processing/min_max_operations.h"
#include "common_audio/signal_processing/cross_correlation.h"
#include "common_audio/signal_processing/downsample_fast.h"
#include "common_audio/signal_processing/dot_product_with_scale.h"
#include "common_audio/third_party/spl_sqrt_floor/spl_sqrt_floor.h"
#include "modules/audio_coding/neteq/audio_vector.h"
#include "modules/audio_coding/neteq/audio_multi_vector.h"
#include "modules/audio_coding/neteq/sync_buffer.h"
#include "modules/audio_coding/neteq/dsp_helper.h"
#include "modules/audio_coding/neteq/cross_correlation.h"
#include "modules/audio_coding/neteq/random_vector.h"

using namespace webrtc;

struct Rng {
  uint32_t s;
  uint32_t next() { s ^= s << 13; s ^= s >> 17; s ^= s << 5; return s; }
  uint32_t below(uint32_t n) { return next() % n; }
  int range(int lo, int hi) { return lo + (int)below((uint32_t)(hi - lo + 1)); }
  int16_t i16() {
    switch (below(8)) {
      case 0: return -32768;
      case 1: return 32767;
      case 2: case 3: return (int16_t)next();
      default: return (int16_t)((int32_t)below(2001) - 1000);
    }
  }
  int32_t i32() {
    switch (below(6)) {
      case 0: return INT32_MIN;
      case 1: return INT32_MAX;
      case 2: case 3: return (int32_t)next();
      default: return (int32_t)below(200001) - 100000;
    }
  }
  void fill16(int16_t* v, size_t n) { for (size_t i = 0; i < n; ++i) v[i] = i16(); }
  void fill32(int32_t* v, size_t n) { for (size_t i = 0; i < n; ++i) v[i] = i32(); }
};
static const uint32_t kSeed = 0x9e3779b9u;

struct Hash {
  uint64_t h = 0xcbf29ce484222325ULL;
  void feed(int64_t v) { h ^= (uint64_t)v; h *= 0x100000001b3ULL; }
  void feed16(const int16_t* v, size_t n) { for (size_t i = 0; i < n; ++i) feed(v[i]); }
  void feed32(const int32_t* v, size_t n) { for (size_t i = 0; i < n; ++i) feed(v[i]); }
};
static void report(const char* name, const Hash& h) {
  printf("%s 0x%016llx\n", name, (unsigned long long)h.h);
}

static void inline_ops() {
  Rng r{kSeed}; Hash h;
  for (int n = 0; n < 4000; ++n) {
    int32_t x = r.i32(), y = r.i32();
    h.feed(WebRtcSpl_NormW32(x));
    h.feed(WebRtcSpl_NormW16((int16_t)x));
    h.feed(WebRtcSpl_NormU32((uint32_t)x));
    h.feed(WebRtcSpl_GetSizeInBits((uint32_t)x));
    h.feed(WebRtcSpl_AddSatW32(x, y));
    h.feed(WebRtcSpl_SubSatW32(x, y));
    h.feed(WebRtcSpl_SatW32ToW16(x));
    h.feed(WebRtcSpl_AddSatW16((int16_t)x, (int16_t)y));
    h.feed(WebRtcSpl_SubSatW16((int16_t)x, (int16_t)y));
    h.feed(WebRtcSpl_CountLeadingZeros32((uint32_t)x));
    h.feed(WebRtcSpl_CountLeadingZeros64(((uint64_t)(uint32_t)x << 32) | (uint32_t)y));
  }
  report("inline", h);
}

static void macros() {
  Rng r{kSeed}; Hash h;
  for (int n = 0; n < 4000; ++n) {
    int32_t x = r.i32();
    int c = r.range(-31, 30);
    int16_t a = r.i16(), b = r.i16();
    int s = r.range(0, 15);
    h.feed(WEBRTC_SPL_SHIFT_W32(x, c));
    h.feed(WEBRTC_SPL_ABS_W16((int16_t)x));
    h.feed(WEBRTC_SPL_ABS_W32(x));
    h.feed(WEBRTC_SPL_MUL_16_16(a, b));
    h.feed(WEBRTC_SPL_MUL_16_16_RSFT(a, b, s));
  }
  report("macros", h);
}

static void division() {
  Rng r{kSeed}; Hash h;
  for (int n = 0; n < 4000; ++n) {
    int32_t num = r.i32();
    int16_t den = r.i16();
    if (!(num == INT32_MIN && den == -1)) {
      h.feed(WebRtcSpl_DivW32W16(num, den));
      h.feed(WebRtcSpl_DivW32W16ResW16(num, den));
    }
    h.feed(WebRtcSpl_DivU32U16((uint32_t)num, (uint16_t)den));
    int32_t num2 = (int32_t)(r.next() & 0x7fffffff);
    int16_t hi = (int16_t)r.range(16384, 32767);
    int16_t low = (int16_t)r.range(0, 32767);
    h.feed(WebRtcSpl_DivW32HiLow(num2, hi, low));
  }
  report("division", h);
}

static void sqrt_ops() {
  Rng r{kSeed}; Hash h;
  for (int n = 0; n < 4000; ++n) {
    int32_t x = r.i32();
    h.feed(WebRtcSpl_Sqrt(x));
    h.feed(WebRtcSpl_SqrtFloor(x));
    int32_t x2 = (int32_t)(r.next() >> r.range(0, 31));
    h.feed(WebRtcSpl_Sqrt(x2));
    h.feed(WebRtcSpl_SqrtFloor(x2));
  }
  report("sqrt", h);
}

static void min_max() {
  Rng r{kSeed}; Hash h;
  int16_t v16[64]; int32_t v32[64];
  for (int n = 0; n < 2000; ++n) {
    size_t len = r.range(1, 64);
    r.fill16(v16, len); r.fill32(v32, len);
    h.feed(WebRtcSpl_MaxAbsValueW16C(v16, len));
    h.feed(WebRtcSpl_MaxAbsValueW32C(v32, len));
    h.feed(WebRtcSpl_MaxAbsElementW16(v16, len));
    h.feed(WebRtcSpl_MaxIndexW16(v16, len));
    h.feed(WebRtcSpl_MaxIndexW32(v32, len));
    h.feed(WebRtcSpl_MinIndexW16(v16, len));
    h.feed(WebRtcSpl_MinIndexW32(v32, len));
    h.feed(WebRtcSpl_MaxValueW16C(v16, len));
    h.feed(WebRtcSpl_MaxValueW32C(v32, len));
    h.feed(WebRtcSpl_MinValueW16C(v16, len));
    h.feed(WebRtcSpl_MinValueW32C(v32, len));
    h.feed(WebRtcSpl_MaxAbsIndexW16(v16, len));
    int16_t mn, mx; WebRtcSpl_MinMaxW16(v16, len, &mn, &mx);
    h.feed(mn); h.feed(mx);
  }
  report("min_max", h);
}

// Shared by the SPL and the NetEq cross-correlation tests.
static void cross_corr(bool auto_shift) {
  Rng r{kSeed}; Hash h;
  for (int n = 0; n < 2000; ++n) {
    size_t dim_seq = r.range(1, 40);
    size_t dim_cc = r.range(1, 20);
    int step = r.range(-2, 2);
    int shifts = r.range(0, 12);
    std::vector<int16_t> seq1(dim_seq);
    r.fill16(seq1.data(), dim_seq);
    size_t span = (size_t)std::abs(step) * (dim_cc - 1);
    std::vector<int16_t> seq2(dim_seq + span);
    r.fill16(seq2.data(), seq2.size());
    size_t offset = step < 0 ? span : 0;
    std::vector<int32_t> out(dim_cc);
    if (auto_shift) {
      h.feed(CrossCorrelationWithAutoShift(seq1.data(), seq2.data() + offset, dim_seq, dim_cc,
                                           step, out.data()));
    } else {
      WebRtcSpl_CrossCorrelationC(out.data(), seq1.data(), seq2.data() + offset, dim_seq,
                                  dim_cc, shifts, step);
    }
    h.feed32(out.data(), dim_cc);
  }
  report(auto_shift ? "cross_correlation_with_auto_shift" : "cross_correlation", h);
}

static void downsample() {
  Rng r{kSeed}; Hash h;
  for (int n = 0; n < 2000; ++n) {
    size_t coef_len = r.range(1, 8);
    int factor = r.range(1, 12);
    size_t delay = r.range(0, 4);
    size_t out_len = r.range(0, 10);
    std::vector<int16_t> coefs(coef_len);
    r.fill16(coefs.data(), coef_len);
    size_t offset = coef_len - 1;
    size_t needed = out_len == 0 ? 0 : delay + factor * (out_len - 1) + 1;
    std::vector<int16_t> in(offset + needed + 8);
    r.fill16(in.data(), in.size());
    int in_len = (int)needed + r.range(-2, 3);
    if (in_len < 0) in_len = 0;
    std::vector<int16_t> out(out_len + 1);
    int ret = WebRtcSpl_DownsampleFastC(in.data() + offset, in_len, out.data(), out_len,
                                        coefs.data(), coef_len, factor, delay);
    h.feed(ret);
    if (ret == 0) h.feed16(out.data(), out_len);
  }
  report("downsample_fast", h);
}

static void dot_product() {
  Rng r{kSeed}; Hash h;
  int16_t a[100], b[100];
  for (int n = 0; n < 2000; ++n) {
    size_t len = r.range(0, 100);
    int scaling = r.range(0, 20);
    r.fill16(a, len); r.fill16(b, len);
    h.feed(WebRtcSpl_DotProductWithScale(a, b, len, scaling));
  }
  report("dot_product_with_scale", h);
}

static void vector_scaling() {
  Rng r{kSeed}; Hash h;
  int16_t in1[32], in2[32], out[32]; int32_t in32[32];
  for (int n = 0; n < 2000; ++n) {
    size_t len = r.range(1, 32);
    r.fill16(in1, len); r.fill16(in2, len); r.fill32(in32, len);

    WebRtcSpl_VectorBitShiftW16(out, len, in1, (int16_t)r.range(-3, 15));
    h.feed16(out, len);
    WebRtcSpl_VectorBitShiftW32ToW16(out, len, in32, r.range(-4, 24));
    h.feed16(out, len);
    int16_t gain = r.i16(); int16_t rs = (int16_t)r.range(0, 15);
    WebRtcSpl_ScaleVector(in1, out, gain, len, rs);
    h.feed16(out, len);
    WebRtcSpl_ScaleVectorWithSat(in1, out, gain, len, rs);
    h.feed16(out, len);
    int16_t g1 = r.i16(), g2 = r.i16(); int s1 = r.range(0, 15), s2 = r.range(0, 15);
    WebRtcSpl_ScaleAndAddVectors(in1, g1, s1, in2, g2, s2, out, len);
    h.feed16(out, len);
    int rrs = r.range(-1, 15);
    int ret = WebRtcSpl_ScaleAndAddVectorsWithRoundC(in1, g1, in2, g2, rrs, out, len);
    h.feed(ret);
    if (ret == 0) h.feed16(out, len);
    int32_t add = r.i32(); int16_t ars = (int16_t)r.range(0, 15);
    WebRtcSpl_AffineTransformVector(out, in1, gain, add, ars, len);
    h.feed16(out, len);
    r.fill16(out, len);
    WebRtcSpl_AddAffineVectorToVector(out, in2, gain, add, ars, len);
    h.feed16(out, len);
    WebRtcSpl_ElementwiseVectorMult(out, in1, in2, len, (int16_t)r.range(0, 15));
    h.feed16(out, len);
  }
  report("vector_scaling", h);
}

static void filters() {
  Rng r{kSeed}; Hash h;
  for (int n = 0; n < 2000; ++n) {
    // FilterARFastQ12
    size_t coef_len = r.range(2, 8), data_len = r.range(1, 40);
    std::vector<int16_t> coefs(coef_len), in(data_len), out(coef_len - 1 + data_len);
    r.fill16(coefs.data(), coef_len); r.fill16(in.data(), data_len); r.fill16(out.data(), out.size());
    WebRtcSpl_FilterARFastQ12(in.data(), out.data() + coef_len - 1, coefs.data(), coef_len, data_len);
    h.feed16(out.data(), out.size());
    // FilterMAFastQ12
    size_t b_len = r.range(1, 8), len = r.range(1, 40);
    std::vector<int16_t> b(b_len), min_in(b_len - 1 + len), mout(len);
    r.fill16(b.data(), b_len); r.fill16(min_in.data(), min_in.size());
    WebRtcSpl_FilterMAFastQ12(min_in.data() + b_len - 1, mout.data(), b.data(), b_len, len);
    h.feed16(mout.data(), len);
    // FilterAR
    size_t a_len = r.range(1, 8);
    size_t state_len = r.range(std::max<int>((int)a_len - 1, 1), (int)a_len + 3);
    size_t x_len = r.range(std::max<int>((int)a_len - 1, 1), 30);
    std::vector<int16_t> a(a_len), x(x_len), st(state_len), stl(state_len), f(x_len), fl(x_len);
    r.fill16(a.data(), a_len); r.fill16(x.data(), x_len);
    r.fill16(st.data(), state_len); r.fill16(stl.data(), state_len);
    h.feed(WebRtcSpl_FilterAR(a.data(), a_len, x.data(), x_len, st.data(), state_len, stl.data(),
                              f.data(), fl.data()));
    h.feed16(f.data(), x_len); h.feed16(fl.data(), x_len);
    h.feed16(st.data(), state_len); h.feed16(stl.data(), state_len);
  }
  report("filters", h);
}

static void lpc() {
  Rng r{kSeed}; Hash h;
  for (int n = 0; n < 2000; ++n) {
    size_t len = r.range(1, 80);
    std::vector<int16_t> v(len);
    int amp_shift = r.range(0, 12);
    for (auto& s : v) s = (int16_t)(r.i16() >> amp_shift);
    size_t order = r.range(0, (int)std::min<size_t>(len, 20));
    int32_t ac[21] = {0}; int scale = 0;
    h.feed(WebRtcSpl_AutoCorrelation(v.data(), len, order, ac, &scale));
    h.feed(scale);
    h.feed32(ac, order + 1);

    size_t lorder = r.range(1, 14);
    int32_t R[15] = {0};
    if (r.below(2) == 0) {
      size_t l2 = r.range((int)lorder + 1, 80);
      std::vector<int16_t> w(l2);
      for (auto& s : w) s = (int16_t)(r.i16() >> amp_shift);
      WebRtcSpl_AutoCorrelation(w.data(), l2, lorder, R, &scale);
    } else {
      R[0] = (int32_t)(r.next() & 0x7fffffff);
      for (size_t i = 1; i <= lorder; ++i) R[i] = r.i32();
    }
    int16_t A[15] = {0}, K[15] = {0};
    h.feed(WebRtcSpl_LevinsonDurbin(R, A, K, lorder));
    h.feed16(A, 15); h.feed16(K, 15);

    int en_scale = 0;
    h.feed(WebRtcSpl_Energy(v.data(), len, &en_scale));
    h.feed(en_scale);
    h.feed(WebRtcSpl_GetScalingSquare(v.data(), len, r.range(1, 1000)));

    size_t hsize = r.range(1, 600);
    std::vector<int16_t> win(hsize);
    WebRtcSpl_GetHanningWindow(win.data(), hsize);
    h.feed16(win.data(), hsize);
  }
  report("lpc", h);
}

static void rand_ops() {
  Rng r{kSeed}; Hash h;
  for (int n = 0; n < 2000; ++n) {
    uint32_t seed = r.next();
    for (int i = 0; i < 8; ++i) { h.feed(WebRtcSpl_RandU(&seed)); h.feed(WebRtcSpl_RandN(&seed)); }
    int16_t v[5];
    h.feed(WebRtcSpl_RandUArray(v, 5, &seed));
    h.feed16(v, 5); h.feed(seed);
  }
  report("rand", h);
}

// ---- NetEq vectors ----

static void feed_vec(Hash& h, const AudioVector& v) {
  h.feed((int64_t)v.Size());
  for (size_t i = 0; i < v.Size(); ++i) h.feed(v[i]);
}
static std::vector<int16_t> rand_vec(Rng& r, int max_len) {
  std::vector<int16_t> v(r.range(0, max_len));
  r.fill16(v.data(), v.size());
  return v;
}

static void audio_vector_ops() {
  Rng r{kSeed}; Hash h;
  for (int seq = 0; seq < 300; ++seq) {
    AudioVector vec_default;
    AudioVector vec_sized(r.range(0, 20));
    AudioVector& vec = r.below(2) ? vec_default : vec_sized;
    AudioVector other;
    { auto d = rand_vec(r, 30); other.PushBack(d.data(), d.size()); }
    for (int op = 0; op < 40; ++op) {
      size_t size = vec.Size();
      switch (r.below(20)) {
        case 0: { auto d = rand_vec(r, 30); vec.PushBack(d.data(), d.size()); break; }
        case 1: { auto d = rand_vec(r, 30); vec.PushFront(d.data(), d.size()); break; }
        case 2: vec.PushBack(other); break;
        case 3: vec.PushFront(other); break;
        case 4: {
          size_t pos = r.below(other.Size() + 1);
          size_t len = r.below(other.Size() - pos + 1);
          vec.PushBack(other, len, pos);
          break;
        }
        case 5: vec.PopFront(r.below(size + 5)); break;
        case 6: vec.PopBack(r.below(size + 5)); break;
        case 7: vec.Extend(r.below(20)); break;
        case 8: { auto d = rand_vec(r, 20); vec.InsertAt(d.data(), d.size(), r.below(size + 5)); break; }
        case 9: { size_t len = r.below(20); vec.InsertZerosAt(len, r.below(size + 5)); break; }
        case 10: {
          size_t len = r.below(other.Size() + 1);
          vec.OverwriteAt(other, len, r.below(size + 5));
          break;
        }
        case 11: { auto d = rand_vec(r, 20); vec.OverwriteAt(d.data(), d.size(), r.below(size + 5)); break; }
        case 12: vec.CrossFade(other, r.below(std::min(size, other.Size()) + 1)); break;
        case 13: vec.CopyTo(&other); break;
        case 14: if (size > 0) { size_t i = r.below(size); vec[i] = r.i16(); } break;
        case 15: vec.Clear(); break;
        case 16: {
          size_t pos = r.below(size + 2);
          std::vector<int16_t> dst(r.below(10));
          bool ok = pos <= size && vec.CopyTo(pos, MonoView<int16_t>(dst.data(), dst.size()));
          h.feed(ok);
          if (ok) h.feed16(dst.data(), dst.size());
          break;
        }
        case 17: {
          size_t len = r.below(size + 3);
          size_t pos = r.below(size + 1);
          std::vector<int16_t> dst(len, 0);
          vec.CopyTo(len, pos, dst.data());
          h.feed16(dst.data(), dst.size());
          break;
        }
        case 18: { other.Clear(); auto d = rand_vec(r, 30); other.PushBack(d.data(), d.size()); break; }
        case 19: vec.CopyTo(&vec); break;  // Aliased self-copy.
      }
      feed_vec(h, vec);
    }
  }
  report("audio_vector_ops", h);
}

static void feed_multi(Hash& h, const AudioMultiVector& v) {
  h.feed((int64_t)v.Channels());
  for (size_t c = 0; c < v.Channels(); ++c) feed_vec(h, v[c]);
}
static void fill_multi(Rng& r, AudioMultiVector& v, int max_len) {
  v.Clear();
  std::vector<int16_t> d(r.range(0, max_len) * v.Channels());
  r.fill16(d.data(), d.size());
  v.PushBackInterleaved(d);
}

static void audio_multi_vector_ops() {
  Rng r{kSeed}; Hash h;
  for (int seq = 0; seq < 300; ++seq) {
    size_t ch = r.range(1, 3);
    AudioMultiVector vec_default(ch);
    AudioMultiVector vec_sized(ch, r.range(0, 20));
    AudioMultiVector& vec = r.below(2) ? vec_default : vec_sized;
    AudioMultiVector other(ch);
    fill_multi(r, other, 30);
    for (int op = 0; op < 40; ++op) {
      size_t size = vec.Size();
      switch (r.below(16)) {
        case 0: {
          std::vector<int16_t> d(r.range(0, 30) * ch);
          r.fill16(d.data(), d.size());
          vec.PushBackInterleaved(d);
          break;
        }
        case 1: vec.PushBack(other); break;
        case 2: if (other.Size() > 0) vec.PushBackFromIndex(other, r.below(other.Size())); break;
        case 3: vec.PopFront(r.below(size + 5)); break;
        case 4: vec.PopBack(r.below(size + 5)); break;
        case 5: vec.Zeros(r.below(30)); break;
        case 6: vec.AssertSize(r.below(size + 20)); break;
        case 7: {
          size_t len = r.below(other.Size() + 1);
          vec.OverwriteAt(other, len, r.below(size + 5));
          break;
        }
        case 8: vec.CrossFade(other, r.below(std::min(size, other.Size()) + 1)); break;
        case 9: {
          // Distinct channels only: after a self-copy (see audio_vector_ops)
          // the channels differ in size and later reads are UB in C++.
          if (ch > 1) {
            size_t from = r.below(ch);
            size_t to = (from + 1 + r.below(ch - 1)) % ch;
            vec.CopyChannel(from, to);
          }
          break;
        }
        case 10: {
          size_t len = r.below(size + 3);
          std::vector<int16_t> dst((size + 3) * ch, 0);
          h.feed((int64_t)vec.ReadInterleaved(len, dst.data()));
          h.feed16(dst.data(), dst.size());
          break;
        }
        case 11: {
          size_t start = r.below(size + 1);
          size_t len = r.below(size + 3);
          std::vector<int16_t> dst((size + 3) * ch, 0);
          h.feed((int64_t)vec.ReadInterleavedFromIndex(start, len, dst.data()));
          h.feed16(dst.data(), dst.size());
          break;
        }
        case 12: {
          size_t start = r.below(size + 2);
          size_t spc = r.below(10);
          std::vector<int16_t> dst(spc * ch, 0);
          bool ok = vec.ReadInterleavedFromIndex(start, InterleavedView<int16_t>(dst.data(), spc, ch));
          h.feed(ok);
          if (ok) h.feed16(dst.data(), dst.size());
          break;
        }
        case 13: {
          size_t len = r.below(size + 3);
          std::vector<int16_t> dst((size + 3) * ch, 0);
          h.feed((int64_t)vec.ReadInterleavedFromEnd(len, dst.data()));
          h.feed16(dst.data(), dst.size());
          break;
        }
        case 14: vec.CopyTo(&other); break;
        case 15: fill_multi(r, other, 30); break;
      }
      feed_multi(h, vec);
    }
  }
  report("audio_multi_vector_ops", h);
}

static void sync_buffer_ops() {
  Rng r{kSeed}; Hash h;
  for (int seq = 0; seq < 300; ++seq) {
    size_t ch = r.range(1, 3);
    SyncBuffer sb(ch, r.range(0, 40));
    AudioMultiVector other(ch);
    for (int op = 0; op < 40; ++op) {
      size_t size = sb.Size();
      switch (r.below(13)) {
        case 0: fill_multi(r, other, 20); sb.PushBack(other); break;
        case 1: {
          std::vector<int16_t> d(r.range(0, 20) * ch);
          r.fill16(d.data(), d.size());
          sb.PushBackInterleaved(BufferT<int16_t>(d.data(), d.size()));
          break;
        }
        case 2: sb.PushFrontZeros(r.below(size + 5)); break;
        case 3: { size_t len = r.below(size + 5); sb.InsertZerosAtIndex(len, r.below(size + 5)); break; }
        case 4: {
          fill_multi(r, other, 20);
          size_t len = r.below(other.Size() + 1);
          sb.ReplaceAtIndex(other, len, r.below(size + 5));
          break;
        }
        case 5: fill_multi(r, other, 20); sb.ReplaceAtIndex(other, r.below(size + 5)); break;
        case 6: {
          size_t spc = r.below(15);
          std::vector<int16_t> dst(spc * ch, 0);
          bool ok = sb.GetNextAudioInterleaved(InterleavedView<int16_t>(dst.data(), spc, ch));
          h.feed(ok);
          if (ok) h.feed16(dst.data(), dst.size());
          break;
        }
        case 7: sb.set_next_index(r.below(size + 5)); break;
        case 8: sb.set_dtmf_index(r.below(size + 5)); break;
        case 9: sb.IncreaseEndTimestamp(r.next()); break;
        case 10: sb.Flush(); break;
        case 11: sb.set_end_timestamp(r.next()); break;
        case 12: h.feed((int64_t)sb.FutureLength()); break;
      }
      h.feed((int64_t)sb.next_index());
      h.feed((int64_t)sb.dtmf_index());
      h.feed(sb.end_timestamp());
      feed_multi(h, sb);
    }
  }
  report("sync_buffer_ops", h);
}

static const int kFsMult[4] = {1, 2, 4, 6};
static const int kRates[4] = {8000, 16000, 32000, 48000};

static void dsp_helper() {
  Rng r{kSeed}; Hash h;
  for (int n = 0; n < 2000; ++n) {
    {  // RampSignal (array + in place + multi)
      size_t len = r.range(0, 50);
      int factor = r.range(0, 16384);
      int inc = r.range(-20000, 20000);
      std::vector<int16_t> in(len), out(len);
      r.fill16(in.data(), len);
      h.feed(DspHelper::RampSignal(in.data(), len, factor, inc, out.data()));
      h.feed16(out.data(), len);
      h.feed(DspHelper::RampSignal(in.data(), len, factor, inc));
      h.feed16(in.data(), len);
      size_t ch = r.range(1, 3);
      AudioMultiVector mv(ch);
      fill_multi(r, mv, 60);
      size_t start = r.below(mv.Size() + 2);
      size_t rlen = r.below(mv.Size() + 2);
      h.feed(DspHelper::RampSignal(&mv, start, rlen, factor, inc));
      feed_multi(h, mv);
    }
    {  // PeakDetection
      size_t num_peaks = r.range(1, 3);
      size_t len = r.range(std::max<int>(2, (int)num_peaks), 60);
      int fs_mult = kFsMult[r.below(4)];
      std::vector<int16_t> data(len);
      r.fill16(data.data(), len);
      size_t idx[3] = {0}; int16_t val[3] = {0};
      DspHelper::PeakDetection(data.data(), len, num_peaks, fs_mult, idx, val);
      for (size_t i = 0; i < num_peaks; ++i) { h.feed((int64_t)idx[i]); h.feed(val[i]); }
      h.feed16(data.data(), len);
    }
    {  // ParabolicFit
      int16_t pts[3] = {r.i16(), r.i16(), r.i16()};
      int fs_mult = kFsMult[r.below(4)];
      size_t idx = r.range(0, 100); int16_t val = 0;
      DspHelper::ParabolicFit(pts, fs_mult, &idx, &val);
      h.feed((int64_t)idx); h.feed(val);
    }
    {  // MinDistortion
      size_t max_lag = r.range(1, 20), min_lag = r.range(0, (int)max_lag);
      size_t len = r.range(1, 40);
      std::vector<int16_t> sig(max_lag + len);
      r.fill16(sig.data(), sig.size());
      int32_t dist = 0;
      h.feed((int64_t)DspHelper::MinDistortion(sig.data() + max_lag, min_lag, max_lag, len, &dist));
      h.feed(dist);
    }
    {  // CrossFade
      size_t len = r.range(0, 40);
      std::vector<int16_t> a(len), b(len), out(len);
      r.fill16(a.data(), len); r.fill16(b.data(), len);
      int16_t mix = (int16_t)r.range(0, 16384);
      int16_t dec = (int16_t)r.range(0, 2000);
      DspHelper::CrossFade(a.data(), b.data(), len, &mix, dec, out.data());
      h.feed(mix); h.feed16(out.data(), len);
    }
    {  // UnmuteSignal + MuteSignal
      size_t len = r.range(0, 40);
      std::vector<int16_t> in(len), out(len);
      r.fill16(in.data(), len);
      int16_t factor = r.i16();
      int inc = r.range(-5000, 70000);
      DspHelper::UnmuteSignal(in.data(), len, &factor, inc, out.data());
      h.feed(factor); h.feed16(out.data(), len);
      DspHelper::MuteSignal(in.data(), r.range(-1000, 30000), len);
      h.feed16(in.data(), len);
    }
    {  // DownsampleTo4kHz
      int rate = kRates[r.below(4)];
      bool comp = r.below(2) == 1;
      size_t out_len = r.range(1, 20);
      size_t in_len = r.range(7, 300);
      std::vector<int16_t> in(310), out(out_len);
      r.fill16(in.data(), in.size());
      int ret = DspHelper::DownsampleTo4kHz(in.data(), in_len, out_len, rate, comp, out.data());
      h.feed(ret);
      if (ret == 0) h.feed16(out.data(), out_len);
    }
  }
  report("dsp_helper", h);
}

static void random_vector_ops() {
  Rng r{kSeed}; Hash h;
  RandomVector rv;
  int16_t out[50];
  for (int n = 0; n < 4000; ++n) {
    switch (r.below(5)) {
      case 0: case 1: { size_t len = r.below(50); rv.Generate(len, out); h.feed16(out, len); break; }
      case 2: rv.IncreaseSeedIncrement((int16_t)r.range(-300, 300)); break;
      case 3: rv.set_seed_increment(r.i16()); break;
      case 4: if (r.below(10) == 0) rv.Reset(); break;
    }
    h.feed(rv.seed_increment());
  }
  report("random_vector_ops", h);
}

int main() {
  inline_ops(); macros(); division(); sqrt_ops(); min_max(); cross_corr(false); downsample();
  dot_product(); vector_scaling(); filters(); lpc(); rand_ops();
  cross_corr(true); audio_vector_ops(); audio_multi_vector_ops(); sync_buffer_ops();
  dsp_helper(); random_vector_ops();
  return 0;
}
