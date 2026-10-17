//! Narrow ownership-safe wrapper over the pinned segmentation model.
//!
//! The C++ bridge uses the same sherpa/ONNX runtime already linked by
//! sherpa-onnx-sys. No additional runtime or modified native archive is used.

use std::ffi::{c_char, c_void, CString};
use std::ptr::NonNull;

use super::{pipeline, DiarizationError};

#[repr(C)]
#[derive(Default)]
struct NativeMetadata {
    sample_rate: i32,
    window_size: i32,
    window_shift: i32,
    receptive_field_size: i32,
    receptive_field_shift: i32,
    num_speakers: i32,
    powerset_max_classes: i32,
    num_classes: i32,
}

unsafe extern "C" {
    fn kuali_segmentation_create(
        path: *const c_char,
        threads: i32,
        metadata: *mut NativeMetadata,
    ) -> *mut c_void;
    fn kuali_segmentation_destroy(model: *mut c_void);
    fn kuali_segmentation_run(
        model: *const c_void,
        samples: *const f32,
        sample_count: i32,
        output: *mut f32,
        capacity: i32,
    ) -> i32;
}

pub(super) struct SegmentationModel {
    ptr: NonNull<c_void>,
    metadata: pipeline::Metadata,
    output_len: usize,
}

impl SegmentationModel {
    pub(super) fn load(path: &str, threads: i32) -> Result<Self, DiarizationError> {
        let path = CString::new(path).map_err(|_| DiarizationError::Initialization)?;
        let mut raw = NativeMetadata::default();
        // SAFETY: path is NUL-terminated and lives through creation. The bridge
        // copies its contents, writes exactly NativeMetadata, catches C++
        // exceptions, and returns either an owned model or a null pointer.
        let ptr = unsafe { kuali_segmentation_create(path.as_ptr(), threads, &mut raw) };
        let ptr = NonNull::new(ptr).ok_or(DiarizationError::Initialization)?;
        // These dimensions belong to the SHA-pinned segmentation export.
        // Restricting them bounds allocation and prevents interpreting a new
        // powerset encoding using the current mapping by accident.
        if raw.sample_rate != 16_000
            || raw.window_size != 160_000
            || raw.window_shift != 16_000
            || raw.receptive_field_size != 991
            || raw.receptive_field_shift != 270
            || raw.num_speakers != 3
            || raw.powerset_max_classes != 2
            || raw.num_classes != 7
        {
            // SAFETY: creation transferred this valid model to us; no other
            // owner exists and it has not yet been exposed to a caller.
            unsafe { kuali_segmentation_destroy(ptr.as_ptr()) };
            return Err(DiarizationError::Initialization);
        }
        let metadata = pipeline::Metadata {
            sample_rate: raw.sample_rate as usize,
            window_size: raw.window_size as usize,
            window_shift: raw.window_shift as usize,
            receptive_field_size: raw.receptive_field_size as usize,
            receptive_field_shift: raw.receptive_field_shift as usize,
            local_speakers: raw.num_speakers as u8,
        };
        let frames = (metadata.window_size - metadata.receptive_field_size)
            / metadata.receptive_field_shift
            + 1;
        Ok(Self {
            ptr,
            metadata,
            output_len: frames * raw.num_classes as usize,
        })
    }

    pub(super) fn metadata(&self) -> pipeline::Metadata {
        self.metadata
    }

    pub(super) fn process(
        &self,
        samples: &[f32],
    ) -> Result<pipeline::ChunkMasks, DiarizationError> {
        if samples.len() != self.metadata.window_size {
            return Err(DiarizationError::Inference);
        }
        let mut probabilities = vec![0.0; self.output_len];
        // SAFETY: this live model owns its native state; the input and output
        // slices remain allocated for the call. Their lengths match the pinned
        // model and fit i32. The bridge validates output shape before copying.
        let written = unsafe {
            kuali_segmentation_run(
                self.ptr.as_ptr(),
                samples.as_ptr(),
                samples.len() as i32,
                probabilities.as_mut_ptr(),
                probabilities.len() as i32,
            )
        };
        if written != self.output_len as i32 {
            return Err(DiarizationError::Inference);
        }
        pipeline::decode_powerset(&probabilities, 7, self.metadata.local_speakers)
            .map_err(|_| DiarizationError::Inference)
    }
}

impl Drop for SegmentationModel {
    fn drop(&mut self) {
        // SAFETY: this is the unique owner of the pointer returned by create.
        unsafe { kuali_segmentation_destroy(self.ptr.as_ptr()) };
    }
}
