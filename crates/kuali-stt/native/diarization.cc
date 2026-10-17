// Kuali's narrow C ABI around the pinned sherpa-onnx segmentation model.
// The implementation links to the existing sherpa runtime; it never replaces
// runtime objects or modifies downloaded native libraries.
#include <algorithm>
#include <array>
#include <cstdint>
#include <memory>
#include <vector>
#include "fastcluster.h"
#include "sherpa-onnx/csrc/offline-speaker-segmentation-pyannote-model.h"

namespace {
using Model = sherpa_onnx::OfflineSpeakerSegmentationPyannoteModel;
struct Metadata {
  int32_t sample_rate;
  int32_t window_size;
  int32_t window_shift;
  int32_t receptive_field_size;
  int32_t receptive_field_shift;
  int32_t num_speakers;
  int32_t powerset_max_classes;
  int32_t num_classes;
};
}
extern "C" {
int32_t kuali_average_linkage(int32_t n, double *distances, int32_t *merge,
                             double *heights) noexcept {
  if (n < 2 || !distances || !merge || !heights) return -1;
  try {
    return fastclustercpp::hclust_fast(n, distances,
        fastclustercpp::HCLUST_METHOD_AVERAGE, merge, heights);
  } catch (...) {
    return -1;
  }
}
void *kuali_segmentation_create(const char *path, int32_t threads,
                                Metadata *metadata) noexcept {
  if (!path || !metadata || threads < 1) return nullptr;
  try {
    sherpa_onnx::OfflineSpeakerSegmentationModelConfig config;
    config.pyannote.model = path;
    config.pyannote.window_shift_ratio = 0.1f;
    config.num_threads = threads;
    auto model = std::make_unique<Model>(config);
    const auto &m = model->GetModelMetaData();
    *metadata = {m.sample_rate, m.window_size, m.window_shift,
                 m.receptive_field_size, m.receptive_field_shift,
                 m.num_speakers, m.powerset_max_classes, m.num_classes};
    return model.release();
  } catch (...) {
    return nullptr;
  }
}
void kuali_segmentation_destroy(void *ptr) noexcept {
  delete static_cast<Model *>(ptr);
}
int32_t kuali_segmentation_run(const void *ptr, const float *samples,
                               int32_t sample_count, float *output,
                               int32_t capacity) noexcept {
  if (!ptr || !samples || !output || sample_count <= 0 || capacity <= 0) return -1;
  try {
    const auto &model = *static_cast<const Model *>(ptr);
    const auto &m = model.GetModelMetaData();
    if (sample_count != m.window_size) return -1;
    auto memory = Ort::MemoryInfo::CreateCpu(OrtDeviceAllocator, OrtMemTypeDefault);
    const std::array<int64_t, 3> shape = {1, 1, sample_count};
    auto input = Ort::Value::CreateTensor(memory, const_cast<float *>(samples),
                                          sample_count, shape.data(), shape.size());
    auto result = model.Forward(std::move(input));
    auto info = result.GetTensorTypeAndShapeInfo();
    auto dimensions = info.GetShape();
    if (dimensions.size() != 3 || dimensions[0] != 1 || dimensions[1] <= 0 ||
        dimensions[2] != m.num_classes || info.GetElementCount() > static_cast<size_t>(capacity)) return -1;
    std::copy_n(result.GetTensorData<float>(), info.GetElementCount(), output);
    return static_cast<int32_t>(info.GetElementCount());
  } catch (...) {
    return -1;
  }
}
}
