use super::{device_manager::DeviceManager, IRIS_CODE_LENGTH};
use crate::{
    helpers::id_wrapper::{http_root, IdWrapper},
    rng::chacha::ChaChaCudaRng,
};
use axum::{routing::get, Router};
use cudarc::{
    cublas::{result::gemm_ex, sys, CudaBlas},
    driver::{
        result::malloc_async, sys::CUdeviceptr, CudaFunction, CudaSlice, CudaStream, DevicePtr,
        LaunchAsync, LaunchConfig,
    },
    nccl::{self, result, Comm, Id, NcclType},
    nvrtc::compile_ptx,
};
use rayon::prelude::*;
use std::{ffi::c_void, mem, str::FromStr, sync::Arc, thread, time::Duration};
use tokio::task::{AbortHandle, JoinSet};

const PTX_SRC: &str = include_str!("kernel.cu");
const REDUCE_FUNCTION_NAME: &str = "matmul_correct_and_reduce";
const LIMBS: usize = 2;

pub fn preprocess_query(query: &[u16]) -> Vec<Vec<u8>> {
    let mut result = vec![];
    for _ in 0..LIMBS {
        result.push(vec![0u8; query.len()]);
    }

    for (idx, &entry) in query.iter().enumerate() {
        for i in 0..LIMBS {
            let tmp = (entry as u32 >> (i * 8)) as u8;
            result[i][idx] = (tmp as i32 - 128) as u8;
        }
    }

    result.to_vec()
}

#[allow(clippy::too_many_arguments)]
pub fn gemm(
    handle: &CudaBlas,
    a: CUdeviceptr,
    b: CUdeviceptr,
    c: CUdeviceptr,
    a_offset: u64,
    b_offset: u64,
    c_offset: u64,
    m: usize,
    n: usize,
    k: usize,
    alpha: i32,
    beta: i32,
) {
    unsafe {
        gemm_ex(
            *handle.handle(),
            sys::cublasOperation_t::CUBLAS_OP_T,
            sys::cublasOperation_t::CUBLAS_OP_N,
            m as i32,
            n as i32,
            k as i32,
            &alpha as *const i32 as *const c_void,
            (a + a_offset) as *const _,
            sys::cublasDataType_t::CUDA_R_8I,
            k as i32,
            (b + b_offset) as *const _,
            sys::cublasDataType_t::CUDA_R_8I,
            k as i32,
            &beta as *const i32 as *const c_void,
            (c + c_offset) as *mut _,
            sys::cublasDataType_t::CUDA_R_32I,
            m as i32,
            sys::cublasComputeType_t::CUBLAS_COMPUTE_32I_PEDANTIC,
            sys::cublasGemmAlgo_t::CUBLAS_GEMM_DEFAULT,
        )
        .unwrap();
    }
}

fn send_stream<T: NcclType>(
    sendbuff: &CudaSlice<T>,
    len: usize,
    peer: usize,
    comm: &Comm,
    stream: &CudaStream,
) -> Result<result::NcclStatus, result::NcclError> {
    unsafe {
        result::send(
            *sendbuff.device_ptr() as *mut _,
            len,
            T::as_nccl_type(),
            peer as i32,
            comm.comm.0,
            stream.stream as *mut _,
        )
    }
}

fn receive_stream<T: NcclType>(
    recvbuff: &mut CudaSlice<T>,
    len: usize,
    peer: usize,
    comm: &Comm,
    stream: &CudaStream,
) -> Result<result::NcclStatus, result::NcclError> {
    unsafe {
        result::recv(
            *recvbuff.device_ptr() as *mut _,
            len,
            T::as_nccl_type(),
            peer as i32,
            comm.comm.0,
            stream.stream as *mut _,
        )
    }
}

fn chunking<T: Clone>(
    slice: &[T],
    n_chunks: usize,
    chunk_size: usize,
    element_size: usize,
    alternating: bool,
) -> Vec<Vec<T>> {
    if alternating {
        let mut result = vec![Vec::new(); n_chunks];

        for (i, chunk) in slice.chunks(element_size).enumerate() {
            result[i % n_chunks].extend_from_slice(chunk);
        }
        result
    } else {
        slice
            .chunks(chunk_size)
            .map(|chunk| chunk.to_vec())
            .collect()
    }
}

pub struct ShareDB {
    peer_id:              usize,
    is_remote:            bool,
    query_length:         usize,
    device_manager:       Arc<DeviceManager>,
    kernels:              Vec<CudaFunction>,
    rngs:                 Vec<(ChaChaCudaRng, ChaChaCudaRng)>,
    comms:                Vec<Arc<Comm>>,
    ones:                 Vec<CudaSlice<u8>>,
    intermediate_results: Vec<CudaSlice<i32>>,
    pub results:          Vec<CudaSlice<u8>>,
    pub results_peer:     Vec<CudaSlice<u8>>,
    pub server_abort:     Option<AbortHandle>,
}

impl ShareDB {
    #[allow(clippy::too_many_arguments)]
    pub fn init(
        peer_id: usize,
        device_manager: Arc<DeviceManager>,
        max_db_length: usize,
        query_length: usize,
        chacha_seeds: ([u32; 8], [u32; 8]),
        peer_url: Option<String>,
        is_remote: Option<bool>,
        server_port: Option<u16>,
        sever_task_set: Option<&mut JoinSet<()>>,
    ) -> Self {
        let n_devices = device_manager.device_count();
        let ptx = compile_ptx(PTX_SRC).unwrap();
        let is_remote = is_remote.unwrap_or(false);

        let mut kernels = Vec::new();

        for i in 0..n_devices {
            let dev = device_manager.device(i);
            dev.load_ptx(ptx.clone(), REDUCE_FUNCTION_NAME, &[REDUCE_FUNCTION_NAME])
                .unwrap();
            let function = dev
                .get_func(REDUCE_FUNCTION_NAME, REDUCE_FUNCTION_NAME)
                .unwrap();

            kernels.push(function);
        }

        let ones = vec![1u8; IRIS_CODE_LENGTH];
        let ones = (0..n_devices)
            .map(|idx| device_manager.device(idx).htod_sync_copy(&ones).unwrap())
            .collect::<Vec<_>>();

        // TODO: depending on the batch size, intermediate_results can get quite big, we
        // can perform the gemm in chunks to limit this
        let mut intermediate_results = vec![];
        let mut results = vec![];
        let mut results_peer = vec![];
        let results_len = max_db_length / n_devices * query_length;

        for idx in 0..n_devices {
            unsafe {
                intermediate_results.push(device_manager.device(idx).alloc(results_len).unwrap());
                results.push(
                    device_manager
                        .device(idx)
                        .alloc(results_len * std::mem::size_of::<u16>())
                        .unwrap(),
                );
                results_peer.push(
                    device_manager
                        .device(idx)
                        .alloc(results_len * std::mem::size_of::<u16>())
                        .unwrap(),
                );
            }
        }

        // Init RNGs
        let rng_buf_size: usize =
            (max_db_length / n_devices * query_length * mem::size_of::<u16>()).div_ceil(64) * 64;
        let mut rngs = vec![];
        for idx in 0..n_devices {
            let (seed0, seed1) = chacha_seeds;
            let mut chacha1 =
                ChaChaCudaRng::init(rng_buf_size, device_manager.device(idx).clone(), seed0);
            chacha1.get_mut_chacha().set_nonce(idx as u64);
            let mut chacha2 =
                ChaChaCudaRng::init(rng_buf_size, device_manager.device(idx).clone(), seed1);
            chacha2.get_mut_chacha().set_nonce(idx as u64);
            rngs.push((chacha1, chacha2));
        }

        // Init NCCL comms
        let mut comms = vec![];
        let mut server_abort = None;
        if is_remote {
            let mut ids = vec![];
            for _ in 0..n_devices {
                ids.push(Id::new().unwrap());
            }

            // Start HTTP server to exchange NCCL commIds
            if peer_id == 0 {
                let sever_task_set = sever_task_set.expect(
                    "task set must be supplied to peer_id 0 for remote connection monitoring",
                );

                let ids = ids.clone();
                server_abort = Some(sever_task_set.spawn(async move {
                    println!("Starting server on port {}...", server_port.unwrap());
                    let app =
                        Router::new().route("/:device_id", get(move |req| http_root(ids, req)));
                    let listener =
                        tokio::net::TcpListener::bind(format!("0.0.0.0:{}", server_port.unwrap()))
                            .await
                            .unwrap();
                    axum::serve(listener, app).await.unwrap();
                }));
            } else {
                thread::sleep(Duration::from_secs(2));
            }

            for i in 0..n_devices {
                let id = if peer_id == 0 {
                    ids[i]
                } else {
                    let peer_url = peer_url.clone().unwrap();
                    std::thread::spawn(move || {
                        let res = reqwest::blocking::get(format!(
                            "http://{}:{}/{}",
                            peer_url,
                            server_port.unwrap(),
                            i
                        ))
                        .unwrap();
                        IdWrapper::from_str(&res.text().unwrap()).unwrap().0
                    })
                    .join()
                    .unwrap()
                };
                ids.push(id);

                // Bind to thread (important!)
                device_manager.device(i).bind_to_thread().unwrap();
                comms.push(Arc::new(
                    Comm::from_rank(device_manager.device(i), peer_id, 3, id).unwrap(),
                ));
            }
        }

        Self {
            peer_id,
            is_remote,
            query_length,
            device_manager,
            kernels,
            rngs,
            comms,
            intermediate_results,
            ones,
            results,
            results_peer,
            server_abort,
        }
    }

    #[allow(clippy::type_complexity)]
    pub fn load_db(
        &self,
        db_entries: &[u16],
        db_length: usize, // TODO: should handle different sizes for each device
        max_db_length: usize,
        alternating_chunks: bool,
    ) -> (
        (Vec<CudaSlice<i8>>, Vec<CudaSlice<i8>>),
        (Vec<CudaSlice<u32>>, Vec<CudaSlice<u32>>),
    ) {
        let mut a1_host = db_entries
            .par_iter()
            .map(|&x: &u16| (x >> 8) as i8)
            .collect::<Vec<_>>();
        let mut a0_host = db_entries.par_iter().map(|&x| x as i8).collect::<Vec<_>>();

        // TODO: maybe use gemm here already to speed up loading (we'll need to correct
        // the results as well)
        a1_host
            .par_iter_mut()
            .for_each(|x| *x = (*x as i32 - 128) as i8);

        a0_host
            .par_iter_mut()
            .for_each(|x| *x = (*x as i32 - 128) as i8);

        let a1_sums: Vec<u32> = a1_host
            .par_chunks(IRIS_CODE_LENGTH)
            .map(|row| row.par_iter().map(|&x| x as u32).sum::<u32>())
            .collect();

        let a0_sums: Vec<u32> = a0_host
            .par_chunks(IRIS_CODE_LENGTH)
            .map(|row| row.par_iter().map(|&x| x as u32).sum::<u32>())
            .collect();

        // Split up db and load to all devices
        let chunk_size = db_length / self.device_manager.device_count();
        let max_size = max_db_length / self.device_manager.device_count();

        // DB sums
        let db1_sums = chunking(
            &a1_sums,
            self.device_manager.device_count(),
            chunk_size,
            1,
            alternating_chunks,
        );
        let db0_sums = chunking(
            &a0_sums,
            self.device_manager.device_count(),
            chunk_size,
            1,
            alternating_chunks,
        );

        let db1_sums = db1_sums
            .iter()
            .enumerate()
            .map(|(idx, chunk)| {
                let mut slice = unsafe { self.device_manager.device(idx).alloc(max_size).unwrap() };
                self.device_manager
                    .htod_copy_into(chunk.to_vec(), &mut slice, idx)
                    .unwrap();
                slice
            })
            .collect::<Vec<_>>();
        let db0_sums = db0_sums
            .iter()
            .enumerate()
            .map(|(idx, chunk)| {
                let mut slice = unsafe { self.device_manager.device(idx).alloc(max_size).unwrap() };
                self.device_manager
                    .htod_copy_into(chunk.to_vec(), &mut slice, idx)
                    .unwrap();
                slice
            })
            .collect::<Vec<_>>();

        // DB codes
        let db1 = chunking(
            &a1_host,
            self.device_manager.device_count(),
            chunk_size * IRIS_CODE_LENGTH,
            IRIS_CODE_LENGTH,
            alternating_chunks,
        );

        let db0 = chunking(
            &a0_host,
            self.device_manager.device_count(),
            chunk_size * IRIS_CODE_LENGTH,
            IRIS_CODE_LENGTH,
            alternating_chunks,
        );

        let db1 = db1
            .iter()
            .enumerate()
            .map(|(idx, chunk)| {
                let mut slice = unsafe {
                    self.device_manager
                        .device(idx)
                        .alloc(max_size * IRIS_CODE_LENGTH)
                        .unwrap()
                };
                self.device_manager
                    .htod_copy_into(chunk.to_vec(), &mut slice, idx)
                    .unwrap();
                slice
            })
            .collect::<Vec<_>>();
        let db0 = db0
            .iter()
            .enumerate()
            .map(|(idx, chunk)| {
                let mut slice = unsafe {
                    self.device_manager
                        .device(idx)
                        .alloc(max_size * IRIS_CODE_LENGTH)
                        .unwrap()
                };
                self.device_manager
                    .htod_copy_into(chunk.to_vec(), &mut slice, idx)
                    .unwrap();
                slice
            })
            .collect::<Vec<_>>();

        ((db0, db1), (db0_sums, db1_sums))
    }

    pub fn query_sums(
        &self,
        query_ptrs: &(Vec<CUdeviceptr>, Vec<CUdeviceptr>),
        streams: &[CudaStream],
        blass: &[CudaBlas],
    ) -> (Vec<CUdeviceptr>, Vec<CUdeviceptr>) {
        let mut query1_sums = vec![];
        let mut query0_sums = vec![];

        for idx in 0..self.device_manager.device_count() {
            self.device_manager.device(idx).bind_to_thread().unwrap();

            let query0 = query_ptrs.0[idx];
            let query1 = query_ptrs.1[idx];

            let query0_sum = unsafe {
                malloc_async(
                    streams[idx].stream,
                    self.query_length * mem::size_of::<u32>(),
                )
                .unwrap()
            };

            let query1_sum = unsafe {
                malloc_async(
                    streams[idx].stream,
                    self.query_length * mem::size_of::<u32>(),
                )
                .unwrap()
            };

            gemm(
                &blass[idx],
                query0,
                *self.ones[idx].device_ptr(),
                query0_sum,
                0,
                0,
                0,
                self.query_length,
                1,
                IRIS_CODE_LENGTH,
                1,
                0,
            );
            gemm(
                &blass[idx],
                query1,
                *self.ones[idx].device_ptr(),
                query1_sum,
                0,
                0,
                0,
                self.query_length,
                1,
                IRIS_CODE_LENGTH,
                1,
                0,
            );

            query0_sums.push(query0_sum);
            query1_sums.push(query1_sum);
        }
        (query0_sums, query1_sums)
    }

    pub fn dot(
        &mut self,
        query_ptrs: &(Vec<CUdeviceptr>, Vec<CUdeviceptr>),
        db: &(Vec<CUdeviceptr>, Vec<CUdeviceptr>),
        db_sizes: &[usize],
        streams: &[CudaStream],
        blass: &[CudaBlas],
    ) {
        for idx in 0..self.device_manager.device_count() {
            self.device_manager.device(idx).bind_to_thread().unwrap();
            let query0 = query_ptrs.0[idx];
            let query1 = query_ptrs.1[idx];

            // Prepare randomness to mask results
            if self.is_remote {
                let len: usize = (db_sizes[idx] * self.query_length).div_ceil(64) * 64;
                self.rngs[idx].0.fill_rng_no_host_copy(len, &streams[idx]);
                self.rngs[idx].1.fill_rng_no_host_copy(len, &streams[idx]);
            }

            for (i, d) in [db.0[idx], db.1[idx]].iter().enumerate() {
                for (j, q) in [query0, query1].iter().enumerate() {
                    if i + j >= LIMBS {
                        continue;
                    }
                    gemm(
                        &blass[idx],
                        *d,
                        *q,
                        *self.intermediate_results[idx].device_ptr(),
                        0,
                        0,
                        0,
                        db_sizes[idx],
                        self.query_length,
                        IRIS_CODE_LENGTH,
                        1 << 8 * (i + j),
                        if i + j == 0 { 0 } else { 1 },
                    );
                }
            }
        }
    }

    pub fn dot_reduce(
        &mut self,
        query_sums: &(Vec<CUdeviceptr>, Vec<CUdeviceptr>),
        db_sums: &(Vec<CUdeviceptr>, Vec<CUdeviceptr>),
        db_sizes: &[usize],
        streams: &[CudaStream],
    ) {
        for idx in 0..self.device_manager.device_count() {
            assert!(
                self.rngs[idx].0.cuda_slice().is_some() && self.rngs[idx].1.cuda_slice().is_some()
            );

            let num_elements = db_sizes[idx] * self.query_length;
            let threads_per_block = 256;
            let blocks_per_grid = num_elements.div_ceil(threads_per_block);
            let cfg = LaunchConfig {
                block_dim:        (threads_per_block as u32, 1, 1),
                grid_dim:         (blocks_per_grid as u32, 1, 1),
                shared_mem_bytes: 0,
            };

            unsafe {
                self.kernels[idx]
                    .clone()
                    .launch_on_stream(
                        &streams[idx],
                        cfg,
                        (
                            &self.intermediate_results[idx],
                            &mut self.results[idx],
                            db_sums.0[idx],
                            db_sums.1[idx],
                            query_sums.0[idx],
                            query_sums.1[idx],
                            db_sizes[idx] as u64,
                            (db_sizes[idx] * self.query_length) as u64,
                            self.rngs[idx].0.cuda_slice().unwrap(),
                            self.rngs[idx].1.cuda_slice().unwrap(),
                        ),
                    )
                    .unwrap();
            }
        }
    }

    pub fn reshare_results(&mut self, db_sizes: &[usize], streams: &[CudaStream]) {
        let next_peer = (self.peer_id + 1) % 3;
        let prev_peer = (self.peer_id + 2) % 3;

        nccl::group_start().unwrap();
        for idx in 0..self.device_manager.device_count() {
            send_stream(
                &self.results[idx],
                db_sizes[idx] * self.query_length * 2,
                next_peer,
                &self.comms[idx],
                &streams[idx],
            )
            .unwrap();

            receive_stream(
                &mut self.results_peer[idx],
                db_sizes[idx] * self.query_length * 2,
                prev_peer,
                &self.comms[idx],
                &streams[idx],
            )
            .unwrap();
        }
        nccl::group_end().unwrap();
    }

    pub fn fetch_results(&self, results: &mut [u16], db_sizes: &[usize], device_id: usize) {
        unsafe {
            let res_trans =
                self.results[device_id].transmute(db_sizes[device_id] * self.query_length);

            self.device_manager
                .device(device_id)
                .dtoh_sync_copy_into(&res_trans.unwrap(), results)
                .unwrap();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{preprocess_query, ShareDB};
    use crate::{
        dot::device_manager::DeviceManager,
        helpers::device_ptrs,
        setup::{galois_engine::degree2::GaloisRingIrisCodeShare, iris_db::db::IrisDB},
    };
    use float_eq::assert_float_eq;
    use ndarray::Array2;
    use num_traits::FromPrimitive;
    use rand::{rngs::StdRng, Rng, SeedableRng};
    use std::sync::Arc;
    const WIDTH: usize = 12_800;
    const QUERY_SIZE: usize = 31;
    const DB_SIZE: usize = 8 * 1000;
    const RNG_SEED: u64 = 42;

    /// Helper to generate random ndarray
    fn random_ndarray<T>(array: Vec<u16>, n: usize, m: usize) -> Array2<T>
    where
        T: FromPrimitive,
    {
        Array2::from_shape_vec(
            (n, m),
            array
                .into_iter()
                .map(|x| T::from_u16(x).unwrap())
                .collect::<Vec<_>>(),
        )
        .unwrap()
    }

    /// Helper to generate random vec
    fn random_vec(n: usize, m: usize, max_value: u32) -> Vec<u16> {
        let mut rng = StdRng::seed_from_u64(RNG_SEED);
        (0..n * m)
            .map(|_| rng.gen_range(0..max_value) as u16)
            .collect()
    }

    /// Test to verify the matmul operation for random matrices in the field
    #[test]
    fn check_matmul() {
        let db = random_vec(DB_SIZE, WIDTH, u16::MAX as u32);
        let query = random_vec(QUERY_SIZE, WIDTH, u16::MAX as u32);
        let device_manager = Arc::new(DeviceManager::init());
        let n_devices = device_manager.device_count();
        let mut gpu_result = vec![0u16; DB_SIZE / n_devices * QUERY_SIZE];
        let db_sizes = vec![DB_SIZE / n_devices; n_devices];

        let mut engine = ShareDB::init(
            0,
            device_manager.clone(),
            DB_SIZE,
            QUERY_SIZE,
            ([0u32; 8], [0u32; 8]),
            None,
            None,
            None,
            None,
        );
        let preprocessed_query = preprocess_query(&query);
        let streams = device_manager.fork_streams();
        let blass = device_manager.create_cublas(&streams);
        let preprocessed_query = device_manager.htod_transfer_query(&preprocessed_query, &streams);
        let query_sums = engine.query_sums(&preprocessed_query, &streams, &blass);
        let db_slices = engine.load_db(&db, DB_SIZE, DB_SIZE, false);

        engine.dot(
            &preprocessed_query,
            &(device_ptrs(&db_slices.0 .0), device_ptrs(&db_slices.0 .1)),
            &db_sizes,
            &streams,
            &blass,
        );
        engine.dot_reduce(
            &query_sums,
            &(device_ptrs(&db_slices.1 .0), device_ptrs(&db_slices.1 .1)),
            &db_sizes,
            &streams,
        );
        device_manager.await_streams(&streams);

        let a_nda = random_ndarray::<u16>(db.clone(), DB_SIZE, WIDTH);
        let b_nda = random_ndarray::<u16>(query.clone(), QUERY_SIZE, WIDTH);
        let c_nda = a_nda.dot(&b_nda.t());

        let mut vec_column_major: Vec<u16> = Vec::new();
        for col in 0..c_nda.ncols() {
            for row in c_nda.column(col) {
                vec_column_major.push(*row as u16);
            }
        }

        for device_idx in 0..n_devices {
            engine.fetch_results(&mut gpu_result, &db_sizes, device_idx);
            let selected_elements: Vec<u16> = vec_column_major
                .chunks(DB_SIZE)
                .flat_map(|chunk| {
                    chunk
                        .iter()
                        .skip(DB_SIZE / n_devices * device_idx)
                        .take(DB_SIZE / n_devices)
                })
                .cloned()
                .collect();

            assert_eq!(selected_elements, gpu_result);
        }
    }

    /// Checks that the result of a matmul of the original data equals the
    /// reconstructed result of individual matmuls on the shamir shares.
    #[test]
    fn check_shared_matmul() {
        let mut rng = StdRng::seed_from_u64(RNG_SEED);
        let device_manager = Arc::new(DeviceManager::init());
        let n_devices = device_manager.device_count();

        let db = IrisDB::new_random_par(DB_SIZE, &mut rng);

        let mut gpu_result = vec![
            vec![0u16; DB_SIZE * QUERY_SIZE / n_devices],
            vec![0u16; DB_SIZE * QUERY_SIZE / n_devices],
            vec![0u16; DB_SIZE * QUERY_SIZE / n_devices],
        ];

        let db_sizes = vec![DB_SIZE / n_devices; n_devices];

        for i in 0..3 {
            let device_manager = Arc::clone(&device_manager);

            let codes_db = db
                .db
                .iter()
                .map(|iris| {
                    GaloisRingIrisCodeShare::encode_mask_code(
                        &iris.mask,
                        &mut StdRng::seed_from_u64(RNG_SEED),
                    )[i]
                        .coefs
                })
                .flatten()
                .collect::<Vec<_>>();

            let querys = db.db[0..QUERY_SIZE]
                .iter()
                .map(|iris| {
                    let shares = GaloisRingIrisCodeShare::encode_mask_code(
                        &iris.mask,
                        &mut StdRng::seed_from_u64(RNG_SEED),
                    );
                    GaloisRingIrisCodeShare::preprocess_iris_code_query_shares(shares)[i].coefs
                })
                .flatten()
                .collect::<Vec<_>>();

            let mut engine = ShareDB::init(
                0,
                device_manager.clone(),
                DB_SIZE,
                QUERY_SIZE,
                ([0u32; 8], [0u32; 8]),
                None,
                None,
                None,
                None,
            );
            let preprocessed_query = preprocess_query(&querys);
            let streams = device_manager.fork_streams();
            let blass = device_manager.create_cublas(&streams);
            let preprocessed_query =
                device_manager.htod_transfer_query(&preprocessed_query, &streams);
            let query_sums = engine.query_sums(&preprocessed_query, &streams, &blass);
            let db_slices = engine.load_db(&codes_db, DB_SIZE, DB_SIZE, false);
            engine.dot(
                &preprocessed_query,
                &(device_ptrs(&db_slices.0 .0), device_ptrs(&db_slices.0 .1)),
                &db_sizes,
                &streams,
                &blass,
            );
            engine.dot_reduce(
                &query_sums,
                &(device_ptrs(&db_slices.1 .0), device_ptrs(&db_slices.1 .1)),
                &db_sizes,
                &streams,
            );
            device_manager.await_streams(&streams);
            engine.fetch_results(&mut gpu_result[i], &db_sizes, 0);
        }

        for i in 0..DB_SIZE * QUERY_SIZE / n_devices {
            assert_eq!(
                (gpu_result[0][i] + gpu_result[1][i] + gpu_result[2][i]),
                (db.db[i / (DB_SIZE / n_devices)].mask & db.db[i % (DB_SIZE / n_devices)].mask)
                    .count_ones() as u16
            );
        }
    }

    /// Calculates the distances between a query and a shamir secret shared db
    /// and checks the result against reference plain implementation.
    #[test]
    fn check_shared_distances() {
        let mut rng = StdRng::seed_from_u64(RNG_SEED);
        let device_manager = Arc::new(DeviceManager::init());
        let n_devices = device_manager.device_count();

        let db = IrisDB::new_random_par(DB_SIZE, &mut rng);

        let db_sizes = vec![DB_SIZE / n_devices; n_devices];

        let mut results_codes = [
            vec![0u16; DB_SIZE / n_devices * QUERY_SIZE],
            vec![0u16; DB_SIZE / n_devices * QUERY_SIZE],
            vec![0u16; DB_SIZE / n_devices * QUERY_SIZE],
        ];

        let mut results_masks = [
            vec![0u16; DB_SIZE / n_devices * QUERY_SIZE],
            vec![0u16; DB_SIZE / n_devices * QUERY_SIZE],
            vec![0u16; DB_SIZE / n_devices * QUERY_SIZE],
        ];

        for party_id in 0..3 {
            // DBs
            let codes_db = db
                .db
                .iter()
                .map(|iris| {
                    GaloisRingIrisCodeShare::encode_iris_code(
                        &iris.code,
                        &iris.mask,
                        &mut StdRng::seed_from_u64(RNG_SEED),
                    )[party_id]
                        .coefs
                })
                .flatten()
                .collect::<Vec<_>>();

            let masks_db = db
                .db
                .iter()
                .map(|iris| {
                    GaloisRingIrisCodeShare::encode_mask_code(
                        &iris.mask,
                        &mut StdRng::seed_from_u64(RNG_SEED),
                    )[party_id]
                        .coefs
                })
                .flatten()
                .collect::<Vec<_>>();

            // Queries
            let code_queries = db.db[0..QUERY_SIZE]
                .iter()
                .map(|iris| {
                    let shares = GaloisRingIrisCodeShare::encode_iris_code(
                        &iris.code,
                        &iris.mask,
                        &mut StdRng::seed_from_u64(RNG_SEED),
                    );
                    GaloisRingIrisCodeShare::preprocess_iris_code_query_shares(shares)[party_id]
                        .coefs
                })
                .flatten()
                .collect::<Vec<_>>();

            let mask_queries = db.db[0..QUERY_SIZE]
                .iter()
                .map(|iris| {
                    let shares = GaloisRingIrisCodeShare::encode_mask_code(
                        &iris.mask,
                        &mut StdRng::seed_from_u64(RNG_SEED),
                    );
                    GaloisRingIrisCodeShare::preprocess_iris_code_query_shares(shares)[party_id]
                        .coefs
                })
                .flatten()
                .collect::<Vec<_>>();

            let device_manager = Arc::new(DeviceManager::init());

            let mut codes_engine = ShareDB::init(
                party_id,
                device_manager.clone(),
                DB_SIZE,
                QUERY_SIZE,
                ([0u32; 8], [0u32; 8]),
                None,
                None,
                None,
                None,
            );
            let mut masks_engine = ShareDB::init(
                party_id,
                device_manager.clone(),
                DB_SIZE,
                QUERY_SIZE,
                ([0u32; 8], [0u32; 8]),
                None,
                None,
                None,
                None,
            );

            let code_query = preprocess_query(&code_queries);
            let mask_query = preprocess_query(&mask_queries);

            let streams = device_manager.fork_streams();
            let blass = device_manager.create_cublas(&streams);
            let code_query = device_manager.htod_transfer_query(&code_query, &streams);
            let mask_query = device_manager.htod_transfer_query(&mask_query, &streams);
            let code_query_sums = codes_engine.query_sums(&code_query, &streams, &blass);
            let mask_query_sums = masks_engine.query_sums(&mask_query, &streams, &blass);
            let code_db_slices = codes_engine.load_db(&codes_db, DB_SIZE, DB_SIZE, false);
            let mask_db_slices = codes_engine.load_db(&masks_db, DB_SIZE, DB_SIZE, false);

            codes_engine.dot(
                &code_query,
                &(
                    device_ptrs(&code_db_slices.0 .0),
                    device_ptrs(&code_db_slices.0 .1),
                ),
                &db_sizes,
                &streams,
                &blass,
            );
            masks_engine.dot(
                &mask_query,
                &(
                    device_ptrs(&mask_db_slices.0 .0),
                    device_ptrs(&mask_db_slices.0 .1),
                ),
                &db_sizes,
                &streams,
                &blass,
            );

            codes_engine.dot_reduce(
                &code_query_sums,
                &(
                    device_ptrs(&code_db_slices.1 .0),
                    device_ptrs(&code_db_slices.1 .1),
                ),
                &db_sizes,
                &streams,
            );
            masks_engine.dot_reduce(
                &mask_query_sums,
                &(
                    device_ptrs(&mask_db_slices.1 .0),
                    device_ptrs(&mask_db_slices.1 .1),
                ),
                &db_sizes,
                &streams,
            );

            device_manager.await_streams(&streams);

            // TODO: fetch results also for other devices
            codes_engine.fetch_results(&mut results_codes[party_id], &db_sizes, 0);
            masks_engine.fetch_results(&mut results_masks[party_id], &db_sizes, 0);
        }

        // Reconstruct the results
        let mut reconstructed_codes = vec![];
        let mut reconstructed_masks = vec![];

        for i in 0..results_codes[0].len() {
            let code = results_codes[0][i] + results_codes[1][i] + results_codes[2][i];
            let mask = results_masks[0][i] + results_masks[1][i] + results_masks[2][i];

            if i == 0 {
                println!("Code: {}, Mask: {}", code, mask);
            }

            reconstructed_codes.push(code);
            reconstructed_masks.push(mask);
        }

        // Calculate the distance in plain
        let dists = reconstructed_codes
            .into_iter()
            .zip(reconstructed_masks)
            .map(|(code, mask)| 0.5f64 - (code as i16) as f64 / (2f64 * mask as f64))
            .collect::<Vec<_>>();

        // Compare against plain reference implementation
        let reference_dists = db.calculate_distances(&db.db[0]);

        // TODO: check for all devices and the whole query
        for i in 0..DB_SIZE / n_devices {
            assert_float_eq!(dists[i], reference_dists[i], abs <= 1e-6);
        }
    }
}
