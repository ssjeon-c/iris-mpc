use super::binary::single_extract_msb_u32;
use crate::{
    database_generators::GaloisRingSharedIris,
    execution::session::{BootSession, Session, SessionHandles},
    network::value::NetworkValue::{self},
    protocol::{
        binary::{lift, mul_lift_2k, open_bin},
        prf::{Prf, PrfSeed},
    },
    shares::{bit::Bit, ring_impl::RingElement, share::Share, vecshare::VecShare},
};
use eyre::eyre;

pub(crate) const MATCH_THRESHOLD_RATIO: f64 = iris_mpc_common::iris_db::iris::MATCH_THRESHOLD_RATIO;
pub(crate) const B_BITS: u64 = 16;
pub(crate) const B: u64 = 1 << B_BITS;
pub(crate) const A: u64 = ((1. - 2. * MATCH_THRESHOLD_RATIO) * B as f64) as u64;
pub(crate) const A_BITS: u32 = u64::BITS - A.leading_zeros();

/// Setup the PRF seeds in the replicated protocol.
/// Each party sends to the next party a random seed.
/// At the end, each party will hold two seeds which are the basis of the
/// replicated protocols.
pub async fn setup_replicated_prf(session: &BootSession, my_seed: PrfSeed) -> eyre::Result<Prf> {
    let next_role = session.own_role()?.next(3);
    let prev_role = session.own_role()?.prev(3);
    let network = session.network();
    // send my_seed to the next party
    network
        .send(
            NetworkValue::PrfKey(my_seed).to_network(),
            session.identity(&next_role)?,
            &session.session_id,
        )
        .await?;
    // received other seed from the previous party
    let serialized_other_seed = network
        .receive(session.identity(&prev_role)?, &session.session_id)
        .await;
    // deserializing received seed.
    let other_seed = match NetworkValue::from_network(serialized_other_seed) {
        Ok(NetworkValue::PrfKey(seed)) => seed,
        _ => return Err(eyre!("Could not deserialize PrfKey")),
    };
    // creating the two PRFs
    Ok(Prf::new(my_seed, other_seed))
}

/// Takes as input two code and mask dot products between two Irises: i, j.
/// i.e. code_dot = <i.code, j.code> and mask_dot = <i.mask, j.mask>
/// Then lifts the two dot products to the larger ring (Z_{2^32}), multiplies
/// with some predefined constants B = 2^16
/// A = ((1. - 2. * MATCH_THRESHOLD_RATIO) * B as f64)
/// and then compares mask_dot * A < code_dot * B.
pub async fn compare_threshold(
    session: &mut Session,
    code_dot: Share<u16>,
    mask_dot: Share<u16>,
) -> eyre::Result<Share<Bit>> {
    debug_assert!(A_BITS as u64 <= B_BITS);

    let y = mul_lift_2k::<B_BITS>(&code_dot);
    let mut x = lift::<{ B_BITS as usize }>(session, VecShare::new_vec(vec![mask_dot])).await?;
    debug_assert_eq!(x.len(), 1);
    let mut x = x.pop().expect("Enough elements present");
    x *= A as u32;
    x -= y;

    single_extract_msb_u32::<32>(session, x).await
}

pub(crate) async fn batch_signed_lift(
    session: &mut Session,
    mut pre_lift: VecShare<u16>,
) -> eyre::Result<VecShare<u32>> {
    // Compute (v + 2^{15}) % 2^{16}, to make values positive.
    for v in pre_lift.iter_mut() {
        v.add_assign_const_role(1_u16 << 15, session.own_role()?);
    }
    let mut lifted_values = lift::<16>(session, pre_lift).await?;
    // Now we got shares of d1' over 2^32 such that d1' = (d1'_1 + d1'_2 + d1'_3) %
    // 2^{16} = d1 Next we subtract the 2^15 term we've added previously to
    // get signed shares over 2^{32}
    for v in lifted_values.iter_mut() {
        v.add_assign_const_role(((1_u64 << 32) - (1_u64 << 15)) as u32, session.own_role()?);
    }
    Ok(lifted_values)
}

/// Computes [D1 * T2; D2 * T1] via lifting
pub(crate) async fn cross_mul_via_lift(
    session: &mut Session,
    d1: Share<u16>,
    t1: Share<u16>,
    d2: Share<u16>,
    t2: Share<u16>,
) -> eyre::Result<(Share<u32>, Share<u32>)> {
    let mut pre_lift = VecShare::<u16>::with_capacity(4);
    // Do preprocessing to lift all values
    pre_lift.push(d1);
    pre_lift.push(t2);
    pre_lift.push(d2);
    pre_lift.push(t1);

    let lifted_values = batch_signed_lift(session, pre_lift).await?;

    // Compute d1 * t2; t2 * d1
    let mut exchanged_shares_a = Vec::with_capacity(2);
    let pairs = [
        (
            lifted_values.shares[0].clone(),
            lifted_values.shares[1].clone(),
        ),
        (
            lifted_values.shares[2].clone(),
            lifted_values.shares[3].clone(),
        ),
    ];
    for pair in pairs.iter() {
        let (x, y) = pair;
        let res = session.prf_as_mut().gen_zero_share() + x * y;
        exchanged_shares_a.push(res);
    }

    let network = session.network();
    let next_role = session.identity(&session.own_role()?.next(3))?;
    let prev_role = session.identity(&session.own_role()?.prev(3))?;

    network
        .send(
            NetworkValue::VecRing32(exchanged_shares_a.clone()).to_network(),
            next_role,
            &session.session_id(),
        )
        .await?;

    let serialized_reply = network.receive(prev_role, &session.session_id()).await;
    let res_b = match NetworkValue::from_network(serialized_reply) {
        Ok(NetworkValue::VecRing32(element)) => element,
        _ => return Err(eyre!("Could not deserialize VecRing16")),
    };
    if exchanged_shares_a.len() != res_b.len() {
        return Err(eyre!(
            "Expected a VecRing32 with length {:?} but received with length: {:?}",
            exchanged_shares_a.len(),
            res_b.len()
        ));
    }

    // vec![D1 * T2; T2 * D1]
    let mut res = Vec::with_capacity(2);
    for (a_share, b_share) in exchanged_shares_a.into_iter().zip(res_b) {
        res.push(Share::new(a_share, b_share));
    }

    Ok((res[0].clone(), res[1].clone()))
}

/// Computes (d2*t1 - d1*t2) > 0 by first lifting the values in a batch
/// from Z_{2^16} to a bigger ring Z_{2^32}
/// Does the multiplication in Z_{2^32} and computes the MSB, to check the
/// comparison result.
/// d1, t1 are replicated shares that come from an iris code/mask dot product,
/// ie: d1 = dot(c_x, c_y); t1 = dot(m_x, m_y). d2, t2 are replicated shares
/// that come from an iris code and mask dot product, ie:
/// d2 = dot(c_u, c_w), t2 = dot(m_u, m_w)
pub async fn cross_compare(
    session: &mut Session,
    d1: Share<u16>,
    t1: Share<u16>,
    d2: Share<u16>,
    t2: Share<u16>,
) -> eyre::Result<bool> {
    let (d1t2, d2t1) = cross_mul_via_lift(session, d1, t1, d2, t2).await?;
    let diff = d2t1 - d1t2;
    // Compute bit <- MSB(D2 * T1 - D1 * T2)
    let bit = single_extract_msb_u32::<32>(session, diff).await?;
    // Open bit
    let opened_b = open_bin(session, bit).await?;
    Ok(opened_b.convert())
}

/// Computes the dot product between the iris pairs; for both the code and the
/// mask of the irises. We pack the dot products of the code and mask into one
/// vector to be able to reshare it later.
pub async fn galois_ring_pairwise_distance(
    _session: &mut Session,
    pairs: &[(GaloisRingSharedIris, GaloisRingSharedIris)],
) -> eyre::Result<Vec<RingElement<u16>>> {
    let mut additive_shares = Vec::with_capacity(2 * pairs.len());
    for pair in pairs.iter() {
        let (x, y) = pair;
        let code_dot = x.code.trick_dot(&y.code);
        let mask_dot = x.mask.trick_dot(&y.mask);
        additive_shares.push(RingElement(code_dot));
        // When applying the trick dot on trimmed masks, we have to multiply with 2 the
        // result The intuition being that a GaloisRingTrimmedMask contains half
        // the elements that a full GaloisRingMask has.
        additive_shares.push(RingElement(2) * RingElement(mask_dot));
    }
    Ok(additive_shares)
}

/// Converts additive sharing (from trick_dot output) to a replicated sharing by
/// masking it with a zero sharing
pub async fn galois_ring_to_rep3(
    session: &mut Session,
    items: Vec<RingElement<u16>>,
) -> eyre::Result<Vec<Share<u16>>> {
    let network = session.network().clone();
    let sid = session.session_id();
    let next_party = session.next_identity()?;

    // make sure we mask the input with a zero sharing
    let masked_items: Vec<_> = items
        .iter()
        .map(|x| session.prf_as_mut().gen_zero_share() + x)
        .collect();

    // sending to the next party
    network
        .send(
            NetworkValue::VecRing16(masked_items.clone()).to_network(),
            &next_party,
            &sid,
        )
        .await?;

    // receiving from previous party
    let network = session.network().clone();
    let sid = session.session_id();
    let prev_party = session.prev_identity()?;
    let shares_b = {
        let serialized_other_share = network.receive(&prev_party, &sid).await;
        match NetworkValue::from_network(serialized_other_share) {
            Ok(NetworkValue::VecRing16(message)) => Ok(message),
            _ => Err(eyre!("Error in receiving in galois_ring_to_rep3 operation")),
        }
    }?;
    let res: Vec<Share<u16>> = masked_items
        .into_iter()
        .zip(shares_b)
        .map(|(a, b)| Share::new(a, b))
        .collect();
    Ok(res)
}

/// Checks whether first Iris entry in the pair matches the Iris in the second
/// entry. This is done in the following manner:
/// Compute the dot product between the two Irises.
/// Convert the partial shamir share result to a replicated sharing and then
/// Compare the distance using the MATCH_THRESHOLD_RATIO from the
/// `compare_threshold` function.
pub async fn galois_ring_is_match(
    session: &mut Session,
    pairs: &[(GaloisRingSharedIris, GaloisRingSharedIris)],
) -> eyre::Result<bool> {
    assert_eq!(pairs.len(), 1);
    let additive_dots = galois_ring_pairwise_distance(session, pairs).await?;
    let rep_dots = galois_ring_to_rep3(session, additive_dots).await?;
    // compute dots[0] - dots[1]
    let bit = compare_threshold(session, rep_dots[0].clone(), rep_dots[1].clone()).await?;
    let opened = open_bin(session, bit).await?;
    Ok(opened.convert())
}

/// Checks that the given dot product is zero.
pub async fn is_dot_zero(
    session: &mut Session,
    code_dot: Share<u16>,
    mask_dot: Share<u16>,
) -> eyre::Result<bool> {
    let bit = compare_threshold(session, code_dot, mask_dot).await?;
    let opened = open_bin(session, bit).await?;
    Ok(opened.convert())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        database_generators::generate_galois_iris_shares,
        execution::{local::LocalRuntime, player::Identity},
        hawkers::plaintext_store::PlaintextIris,
        protocol::ops::NetworkValue::RingElement32,
        shares::{int_ring::IntRing2k, ring_impl::RingElement},
    };
    use aes_prng::AesRng;
    use iris_mpc_common::iris_db::db::IrisDB;
    use rand::{Rng, RngCore, SeedableRng};
    use rstest::rstest;
    use std::collections::HashMap;
    use tokio::task::JoinSet;

    async fn open_single(session: &Session, x: Share<u32>) -> eyre::Result<RingElement<u32>> {
        let network = session.network();
        let next_role = session.identity(&session.own_role()?.next(3))?;
        let prev_role = session.identity(&session.own_role()?.prev(3))?;
        network
            .send(
                RingElement32(x.b).to_network(),
                next_role,
                &session.session_id(),
            )
            .await?;
        let serialized_reply = network.receive(prev_role, &session.session_id()).await;
        let missing_share = match NetworkValue::from_network(serialized_reply) {
            Ok(NetworkValue::RingElement32(element)) => element,
            _ => return Err(eyre!("Could not deserialize RingElement32")),
        };
        let (a, b) = x.get_ab();
        Ok(a + b + missing_share)
    }

    async fn open_t_many<T>(session: &Session, shares: Vec<Share<T>>) -> eyre::Result<Vec<T>>
    where
        T: IntRing2k,
        NetworkValue: From<Vec<RingElement<T>>>,
        Vec<RingElement<T>>: TryFrom<NetworkValue, Error = eyre::Error>,
    {
        let next_party = session.next_identity()?;
        let network = session.network().clone();
        let sid = session.session_id();

        let shares_b: Vec<_> = shares.iter().map(|s| s.b).collect();
        let message = shares_b;
        network
            .send(NetworkValue::from(message).to_network(), &next_party, &sid)
            .await?;

        // receiving from previous party
        let network = session.network().clone();
        let sid = session.session_id();
        let prev_party = session.prev_identity()?;
        let shares_c = {
            let serialized_other_share = network.receive(&prev_party, &sid).await;
            let net_message = NetworkValue::from_network(serialized_other_share)?;
            Vec::<RingElement<T>>::try_from(net_message)
        }?;

        let res = shares
            .into_iter()
            .zip(shares_c)
            .map(|(s, c)| {
                let (a, b) = s.get_ab();
                (a + b + c).convert()
            })
            .collect();
        Ok(res)
    }

    #[tokio::test]
    async fn test_async_prf_setup() {
        let num_parties = 3;
        let identities: Vec<Identity> = vec!["alice".into(), "bob".into(), "charlie".into()];
        let mut seeds = Vec::new();
        for i in 0..num_parties {
            let mut seed = [0_u8; 16];
            seed[0] = i;
            seeds.push(seed);
        }
        let local = LocalRuntime::new(identities.clone(), seeds.clone());
        let mut ready_sessions = local.create_player_sessions().await.unwrap();

        // check whether parties have sent/received the correct seeds.
        // P0: [seed_0, seed_2]
        // P1: [seed_1, seed_0]
        // P2: [seed_2, seed_1]
        // This is done by calling next() on the PRFs and see whether they match with
        // the ones created from scratch.
        let prf0 = ready_sessions
            .get_mut(&"alice".into())
            .unwrap()
            .prf_as_mut();
        assert_eq!(
            prf0.get_my_prf().next_u64(),
            Prf::new(seeds[0], seeds[2]).get_my_prf().next_u64()
        );
        assert_eq!(
            prf0.get_prev_prf().next_u64(),
            Prf::new(seeds[0], seeds[2]).get_prev_prf().next_u64()
        );

        let prf1 = ready_sessions.get_mut(&"bob".into()).unwrap().prf_as_mut();
        assert_eq!(
            prf1.get_my_prf().next_u64(),
            Prf::new(seeds[1], seeds[0]).get_my_prf().next_u64()
        );
        assert_eq!(
            prf1.get_prev_prf().next_u64(),
            Prf::new(seeds[1], seeds[0]).get_prev_prf().next_u64()
        );

        let prf2 = ready_sessions
            .get_mut(&"charlie".into())
            .unwrap()
            .prf_as_mut();
        assert_eq!(
            prf2.get_my_prf().next_u64(),
            Prf::new(seeds[2], seeds[1]).get_my_prf().next_u64()
        );
        assert_eq!(
            prf2.get_prev_prf().next_u64(),
            Prf::new(seeds[2], seeds[1]).get_prev_prf().next_u64()
        );
    }

    fn create_single_sharing<R: RngCore>(
        rng: &mut R,
        input: u16,
    ) -> (Share<u16>, Share<u16>, Share<u16>) {
        let a = RingElement(rng.gen::<u16>());
        let b = RingElement(rng.gen::<u16>());
        let c = RingElement(input) - a - b;

        let share1 = Share::new(a, c);
        let share2 = Share::new(b, a);
        let share3 = Share::new(c, b);
        (share1, share2, share3)
    }
    struct LocalShares1D {
        p0: Vec<Share<u16>>,
        p1: Vec<Share<u16>>,
        p2: Vec<Share<u16>>,
    }

    fn create_array_sharing<R: RngCore>(rng: &mut R, input: &Vec<u16>) -> LocalShares1D {
        let mut player0 = Vec::new();
        let mut player1 = Vec::new();
        let mut player2 = Vec::new();

        for entry in input {
            let (a, b, c) = create_single_sharing(rng, *entry);
            player0.push(a);
            player1.push(b);
            player2.push(c);
        }
        LocalShares1D {
            p0: player0,
            p1: player1,
            p2: player2,
        }
    }

    #[tokio::test]
    async fn test_replicated_cross_mul_lift() {
        let mut rng = AesRng::seed_from_u64(0_u64);
        let four_items = vec![1, 2, 3, 4];

        let four_shares = create_array_sharing(&mut rng, &four_items);

        let num_parties = 3;
        let identities: Vec<Identity> = vec!["alice".into(), "bob".into(), "charlie".into()];

        let four_share_map = HashMap::from([
            (identities[0].clone(), four_shares.p0),
            (identities[1].clone(), four_shares.p1),
            (identities[2].clone(), four_shares.p2),
        ]);

        let mut seeds = Vec::new();
        for i in 0..num_parties {
            let mut seed = [0_u8; 16];
            seed[0] = i;
            seeds.push(seed);
        }
        let local = LocalRuntime::new(identities.clone(), seeds.clone());
        let ready_sessions = local.create_player_sessions().await.unwrap();

        let mut jobs = JoinSet::new();
        for player in identities.iter() {
            let mut player_session = ready_sessions.get(player).unwrap().clone();
            let four_shares = four_share_map.get(player).unwrap().clone();
            jobs.spawn(async move {
                let out_shared = cross_mul_via_lift(
                    &mut player_session,
                    four_shares[0].clone(),
                    four_shares[1].clone(),
                    four_shares[2].clone(),
                    four_shares[3].clone(),
                )
                .await
                .unwrap();
                (
                    open_single(&player_session, out_shared.0).await.unwrap(),
                    open_single(&player_session, out_shared.1).await.unwrap(),
                )
            });
        }
        // check first party output is equal to the expected result.
        let t = jobs.join_next().await.unwrap().unwrap();
        assert_eq!(t.0, RingElement(4));
        assert_eq!(t.1, RingElement(6));
    }

    async fn open_additive(session: &Session, x: Vec<RingElement<u16>>) -> eyre::Result<Vec<u16>> {
        let network = session.network();
        let next_role = session.identity(&session.own_role()?.next(3))?;
        let prev_role = session.identity(&session.own_role()?.prev(3))?;
        network
            .send(
                NetworkValue::VecRing16(x.clone()).to_network(),
                next_role,
                &session.session_id(),
            )
            .await?;
        network
            .send(
                NetworkValue::VecRing16(x.clone()).to_network(),
                prev_role,
                &session.session_id(),
            )
            .await?;

        let serialized_reply_0 = network.receive(prev_role, &session.session_id()).await;
        let serialized_reply_1 = network.receive(next_role, &session.session_id()).await;

        let missing_share_0 = match NetworkValue::from_network(serialized_reply_0) {
            Ok(NetworkValue::VecRing16(element)) => element,
            _ => return Err(eyre!("Could not deserialize VecRingElement16")),
        };
        let missing_share_1 = match NetworkValue::from_network(serialized_reply_1) {
            Ok(NetworkValue::VecRing16(element)) => element,
            _ => return Err(eyre!("Could not deserialize VecRingElement16")),
        };
        let opened_value: Vec<u16> = x
            .iter()
            .enumerate()
            .map(|(i, v)| (missing_share_0[i] + missing_share_1[i] + v).convert())
            .collect();
        Ok(opened_value)
    }

    #[tokio::test]
    #[rstest]
    #[case(0)]
    #[case(1)]
    #[case(2)]
    async fn test_galois_ring_to_rep3(#[case] seed: u64) {
        let runtime = LocalRuntime::replicated_test_config();
        let ready_sessions = runtime.create_player_sessions().await.unwrap();
        let mut rng = AesRng::seed_from_u64(seed);

        let iris_db = IrisDB::new_random_rng(2, &mut rng).db;

        let first_entry = generate_galois_iris_shares(&mut rng, iris_db[0].clone());
        let second_entry = generate_galois_iris_shares(&mut rng, iris_db[1].clone());

        let mut jobs = JoinSet::new();
        for (index, player) in runtime.identities.iter().cloned().enumerate() {
            let mut player_session = ready_sessions.get(&player).unwrap().clone();
            let mut own_shares = vec![(first_entry[index].clone(), second_entry[index].clone())];
            own_shares.iter_mut().for_each(|(_x, y)| {
                y.code.preprocess_iris_code_query_share();
                y.mask.preprocess_mask_code_query_share();
            });
            jobs.spawn(async move {
                let x = galois_ring_pairwise_distance(&mut player_session, &own_shares)
                    .await
                    .unwrap();
                let opened_x = open_additive(&player_session, x.clone()).await.unwrap();
                let x_rep = galois_ring_to_rep3(&mut player_session, x).await.unwrap();
                let opened_x_rep = open_t_many(&player_session, x_rep).await.unwrap();
                (opened_x, opened_x_rep)
            });
        }
        let output0 = jobs.join_next().await.unwrap().unwrap();
        let output1 = jobs.join_next().await.unwrap().unwrap();
        let output2 = jobs.join_next().await.unwrap().unwrap();
        assert_eq!(output0, output1);
        assert_eq!(output0, output2);

        let plaintext_first = PlaintextIris(iris_db[0].clone());
        let plaintext_second = PlaintextIris(iris_db[1].clone());
        let (plain_d1, plain_d2) = plaintext_first.dot_distance_fraction(&plaintext_second);
        assert_eq!(output0.0[0], plain_d1 as u16);
        assert_eq!(output0.0[1], plain_d2);

        assert_eq!(output0.1[0], plain_d1 as u16);
        assert_eq!(output0.1[1], plain_d2);
    }
}
