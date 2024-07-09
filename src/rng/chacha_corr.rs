use cudarc::{
    driver::{
        CudaDevice, CudaFunction, CudaSlice, CudaStream, CudaViewMut, DeviceSlice, LaunchAsync,
        LaunchConfig,
    },
    nvrtc::compile_ptx,
};
use std::sync::Arc;

pub struct ChaChaCudaCorrRng {
    // dev:            Arc<CudaDevice>,
    kernels:        [CudaFunction; 2],
    state1_gpu_buf: CudaSlice<u32>,
    state2_gpu_buf: CudaSlice<u32>,
    /// the current state of the chacha rng
    chacha_ctx1:    ChaChaCtx,
    chacha_ctx2:    ChaChaCtx,
}

const CHACHA_PTX_SRC: &str = include_str!("chacha.cu");
const CHACHA_FUNCTION_NAME: &str = "chacha12";
const CHACHA2_FUNCTION_NAME: &str = "chacha12_xor";

impl ChaChaCudaCorrRng {
    // takes number of bytes to produce, buffer has u32 datatype so will produce
    // buf_size/4 u32s
    pub fn init(dev: Arc<CudaDevice>, seed1: [u32; 8], seed2: [u32; 8]) -> Self {
        let ptx = compile_ptx(CHACHA_PTX_SRC).unwrap();

        dev.load_ptx(ptx.clone(), CHACHA_FUNCTION_NAME, &[
            CHACHA_FUNCTION_NAME,
            CHACHA2_FUNCTION_NAME,
        ])
        .unwrap();
        let kernel1 = dev
            .get_func(CHACHA_FUNCTION_NAME, CHACHA_FUNCTION_NAME)
            .unwrap();
        let kernel2 = dev
            .get_func(CHACHA_FUNCTION_NAME, CHACHA2_FUNCTION_NAME)
            .unwrap();
        let chacha_ctx1 = ChaChaCtx::init(seed1, 0, 0);
        let chacha_ctx2 = ChaChaCtx::init(seed2, 0, 0);

        let state1_gpu_buf = dev.htod_sync_copy(chacha_ctx1.state.as_ref()).unwrap();
        let state2_gpu_buf = dev.htod_sync_copy(chacha_ctx2.state.as_ref()).unwrap();

        Self {
            kernels: [kernel1, kernel2],
            chacha_ctx1,
            chacha_ctx2,
            state1_gpu_buf,
            state2_gpu_buf,
        }
    }

    pub fn fill_rng_into(&mut self, buf: &mut CudaViewMut<u32>, stream: &CudaStream) {
        let len = buf.len();
        assert!(len % 16 == 0, "buffer length must be a multiple of 16");
        let num_ks_calls = len / 16; // we produce 16 u32s per kernel call
        let threads_per_block = 256; // todo sync with kernel
        let blocks_per_grid = (num_ks_calls + threads_per_block - 1) / threads_per_block;
        let cfg = LaunchConfig {
            block_dim:        (threads_per_block as u32, 1, 1),
            grid_dim:         (blocks_per_grid as u32, 1, 1),
            shared_mem_bytes: 0,
        };

        unsafe {
            self.kernels[0]
                .clone()
                .launch_on_stream(
                    stream,
                    cfg,
                    (
                        &mut *buf,
                        &self.state1_gpu_buf,
                        self.chacha_ctx1.state[12], // counter part1
                        self.chacha_ctx1.state[13], // counter part2
                        len,
                    ),
                )
                .unwrap();
        }
        // increment the state counter of the ChaChaRng with the number of produced
        // blocks
        let mut counter = self.chacha_ctx1.get_counter();
        counter += num_ks_calls as u64; // one call to KS produces 16 u32, so we increase the counter by the number of
                                        // KS calls
        self.chacha_ctx1.set_counter(counter);

        unsafe {
            self.kernels[1]
                .clone()
                .launch_on_stream(
                    stream,
                    cfg,
                    (
                        buf,
                        &self.state2_gpu_buf,
                        self.chacha_ctx2.state[12], // counter part1
                        self.chacha_ctx2.state[13], // counter part2
                        len,
                    ),
                )
                .unwrap();
        }
        // increment the state counter of the ChaChaRng with the number of produced
        // blocks
        let mut counter = self.chacha_ctx2.get_counter();
        counter += num_ks_calls as u64; // one call to KS produces 16 u32, so we increase the counter by the number of
                                        // KS calls
        self.chacha_ctx2.set_counter(counter);
    }

    pub fn fill_my_rng_into(&mut self, buf: &mut CudaViewMut<u32>, stream: &CudaStream) {
        let len = buf.len();
        assert!(len % 16 == 0, "buffer length must be a multiple of 16");
        let num_ks_calls = len / 16; // we produce 16 u32s per kernel call
        let threads_per_block = 256; // todo sync with kernel
        let blocks_per_grid = (num_ks_calls + threads_per_block - 1) / threads_per_block;
        let cfg = LaunchConfig {
            block_dim:        (threads_per_block as u32, 1, 1),
            grid_dim:         (blocks_per_grid as u32, 1, 1),
            shared_mem_bytes: 0,
        };

        unsafe {
            self.kernels[0]
                .clone()
                .launch_on_stream(
                    stream,
                    cfg,
                    (
                        &mut *buf,
                        &self.state1_gpu_buf,
                        self.chacha_ctx1.state[12], // counter part1
                        self.chacha_ctx1.state[13], // counter part2
                        len,
                    ),
                )
                .unwrap();
        }
        // increment the state counter of the ChaChaRng with the number of produced
        // blocks
        let mut counter = self.chacha_ctx1.get_counter();
        counter += num_ks_calls as u64; // one call to KS produces 16 u32, so we increase the counter by the number of
                                        // KS calls
        self.chacha_ctx1.set_counter(counter);
    }

    pub fn fill_their_rng_into(&mut self, buf: &mut CudaViewMut<u32>, stream: &CudaStream) {
        let len = buf.len();
        assert!(len % 16 == 0, "buffer length must be a multiple of 16");
        let num_ks_calls = len / 16; // we produce 16 u32s per kernel call
        let threads_per_block = 256; // todo sync with kernel
        let blocks_per_grid = (num_ks_calls + threads_per_block - 1) / threads_per_block;
        let cfg = LaunchConfig {
            block_dim:        (threads_per_block as u32, 1, 1),
            grid_dim:         (blocks_per_grid as u32, 1, 1),
            shared_mem_bytes: 0,
        };
        unsafe {
            self.kernels[0]
                .clone()
                .launch_on_stream(
                    stream,
                    cfg,
                    (
                        &mut *buf,
                        &self.state2_gpu_buf,
                        self.chacha_ctx2.state[12], // counter part1
                        self.chacha_ctx2.state[13], // counter part2
                        len,
                    ),
                )
                .unwrap();
        }
        // increment the state counter of the ChaChaRng with the number of produced
        // blocks
        let mut counter = self.chacha_ctx2.get_counter();
        counter += num_ks_calls as u64; // one call to KS produces 16 u32, so we increase the counter by the number of
                                        // KS calls
        self.chacha_ctx2.set_counter(counter);
    }

    pub fn get_mut_chacha(&mut self) -> (&mut ChaChaCtx, &mut ChaChaCtx) {
        (&mut self.chacha_ctx1, &mut self.chacha_ctx2)
    }
    pub fn advance_by_bytes(&mut self, bytes: u64) {
        assert!(bytes % 64 == 0, "bytes must be a multiple of 64");
        let num_ks_calls = bytes / 64; // we produce 16 u32s per kernel call
        let mut counter = self.chacha_ctx1.get_counter();
        counter += num_ks_calls; // one call to KS produces 16 u32s
        self.chacha_ctx1.set_counter(counter);
        let mut counter = self.chacha_ctx2.get_counter();
        counter += num_ks_calls; // one call to KS produces 16 u32s
        self.chacha_ctx2.set_counter(counter);
    }
}

// Modeled after:
// struct chacha_ctx
// {
//     uint32_t keystream[16];
//     uint32_t state[16];
//     uint32_t *counter;
// };

pub struct ChaChaCtx {
    // 12 32-bit words for the key
    // 2 32-bit words for the counter
    // 2 32-bit words for the nonce (stream id)
    pub(crate) state: [u32; 16],
}

const CHACONST: [u32; 4] = [0x61707865, 0x3320646e, 0x79622d32, 0x6b206574];

impl ChaChaCtx {
    const COUNTER_START_IDX: usize = 12;
    const NONCE_START_IDX: usize = 14;
    pub fn init(key: [u32; 8], counter: u64, nonce: u64) -> Self {
        let mut state = [0u32; 16];
        state[0] = CHACONST[0];
        state[1] = CHACONST[1];
        state[2] = CHACONST[2];
        state[3] = CHACONST[3];
        state[4..12].copy_from_slice(&key);

        let mut res = Self { state };
        res.set_counter(counter);
        res.set_nonce(nonce);
        res
    }
    fn get_value(&self, idx: usize) -> u64 {
        self.state[idx] as u64 | ((self.state[idx + 1] as u64) << 32)
    }
    fn set_value(&mut self, idx: usize, value: u64) {
        self.state[idx] = value as u32;
        self.state[idx + 1] = (value >> 32) as u32;
    }

    pub fn set_counter(&mut self, counter: u64) {
        self.set_value(Self::COUNTER_START_IDX, counter)
    }
    pub fn get_counter(&self) -> u64 {
        self.get_value(Self::COUNTER_START_IDX)
    }
    pub fn set_nonce(&mut self, nonce: u64) {
        self.set_value(Self::NONCE_START_IDX, nonce)
    }
    pub fn get_nonce(&self) -> u64 {
        self.get_value(Self::NONCE_START_IDX)
    }
}

#[cfg(test)]
mod tests {

    use super::*;
    use crate::helpers::dtoh_on_stream_sync;
    use itertools::izip;

    #[test]
    fn test_chacha_rng() {
        // This call to CudaDevice::new is only used in context of a test - not used in
        // the server binary
        let dev = CudaDevice::new(0).unwrap();
        let stream = dev.fork_default_stream().unwrap();
        let mut rng = ChaChaCudaCorrRng::init(dev.clone(), [0u32; 8], [1u32; 8]);
        let mut buf = dev.alloc_zeros(1024 * 1024).unwrap();
        rng.fill_rng_into(&mut buf.slice_mut(..), &stream);
        let data = dtoh_on_stream_sync(&buf, &dev, &stream).unwrap();
        let zeros = data.iter().filter(|x| x == &&0).count();
        // we would expect no 0s in the output buffer even 1 is 1/4096;
        assert!(zeros <= 1);
        rng.fill_rng_into(&mut buf.slice_mut(..), &stream);
        let data2 = dtoh_on_stream_sync(&buf, &dev, &stream).unwrap();
        assert!(data != data2);
    }

    #[test]
    fn test_correlation() {
        // This call to CudaDevice::new is only used in context of a test - not used in
        // the server binary
        let dev = CudaDevice::new(0).unwrap();
        let stream = dev.fork_default_stream().unwrap();
        let seed1 = [0u32; 8];
        let seed2 = [1u32; 8];
        let seed3 = [2u32; 8];
        let mut rng1 = ChaChaCudaCorrRng::init(dev.clone(), seed1, seed2);
        let mut rng2 = ChaChaCudaCorrRng::init(dev.clone(), seed2, seed3);
        let mut rng3 = ChaChaCudaCorrRng::init(dev.clone(), seed3, seed1);

        let mut buf = dev.alloc_zeros(1024 * 1024).unwrap();
        rng1.fill_rng_into(&mut buf.slice_mut(..), &stream);
        let data1 = dtoh_on_stream_sync(&buf, &dev, &stream).unwrap();
        rng2.fill_rng_into(&mut buf.slice_mut(..), &stream);
        let data2 = dtoh_on_stream_sync(&buf, &dev, &stream).unwrap();
        rng3.fill_rng_into(&mut buf.slice_mut(..), &stream);
        let data3 = dtoh_on_stream_sync(&buf, &dev, &stream).unwrap();
        for (a, b, c) in izip!(data1, data2, data3) {
            assert_eq!(a ^ b ^ c, 0);
        }
    }
}
