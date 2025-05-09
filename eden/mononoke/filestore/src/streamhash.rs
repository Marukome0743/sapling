/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use futures::future;
use futures::future::FutureExt;
use futures::stream::Stream;
use futures::stream::StreamExt;

use crate::incremental_hash::Hasher;

pub async fn hash_stream<H, I, S>(hasher: impl Hasher<H>, stream: S) -> H
where
    I: AsRef<[u8]>,
    S: Stream<Item = I>,
{
    stream
        .fold(hasher, |mut hasher, bytes| {
            hasher.update(bytes);
            future::ready(hasher)
        })
        .map(|hasher| hasher.finish())
        .await
}

#[cfg(test)]
mod test {
    use bytes::Bytes;
    use futures::stream;
    use mononoke_types::ContentId;
    use mononoke_types::hash::Blake3;
    use mononoke_types::hash::RichGitSha1;
    use mononoke_types::hash::Sha1;
    use mononoke_types::hash::Sha256;

    use super::*;
    use crate::expected_size::ExpectedSize;
    use crate::incremental_hash::Blake3IncrementalHasher;
    use crate::incremental_hash::ContentIdIncrementalHasher;
    use crate::incremental_hash::GitSha1IncrementalHasher;
    use crate::incremental_hash::Sha1IncrementalHasher;
    use crate::incremental_hash::Sha256IncrementalHasher;

    #[tokio::test]
    async fn sha1_simple() {
        let data = Bytes::from(&b"hello, world"[..]); // b7e23ec29af22b0b4e41da31e868d57226121c84
        let s = stream::once(future::ready(data));

        let res: Sha1 = hash_stream(Sha1IncrementalHasher::new(), s).await;

        assert_eq!(
            res,
            Sha1::from_bytes([
                0xb7, 0xe2, 0x3e, 0xc2, 0x9a, 0xf2, 0x2b, 0x0b, 0x4e, 0x41, 0xda, 0x31, 0xe8, 0x68,
                0xd5, 0x72, 0x26, 0x12, 0x1c, 0x84
            ])
            .unwrap()
        );
    }

    #[tokio::test]
    async fn sha1_chunks() {
        let data = vec![&b"hello"[..], &b", "[..], &b"world"[..]] // b7e23ec29af22b0b4e41da31e868d57226121c84
            .into_iter()
            .map(Bytes::from);
        let s = stream::iter(data);

        let res: Sha1 = hash_stream(Sha1IncrementalHasher::new(), s).await;

        assert_eq!(
            res,
            Sha1::from_bytes([
                0xb7, 0xe2, 0x3e, 0xc2, 0x9a, 0xf2, 0x2b, 0x0b, 0x4e, 0x41, 0xda, 0x31, 0xe8, 0x68,
                0xd5, 0x72, 0x26, 0x12, 0x1c, 0x84
            ])
            .unwrap()
        );
    }

    #[tokio::test]
    async fn git_sha1_simple() {
        let data = Bytes::from(&b"hello, world"[..]); // 8c01d89ae06311834ee4b1fab2f0414d35f01102
        let s = stream::once(future::ready(data));

        let res: RichGitSha1 =
            hash_stream(GitSha1IncrementalHasher::new(ExpectedSize::new(12)), s).await;

        assert_eq!(
            res,
            RichGitSha1::from_bytes(
                [
                    0x8c, 0x01, 0xd8, 0x9a, 0xe0, 0x63, 0x11, 0x83, 0x4e, 0xe4, 0xb1, 0xfa, 0xb2,
                    0xf0, 0x41, 0x4d, 0x35, 0xf0, 0x11, 0x02
                ],
                "blob",
                12
            )
            .unwrap()
        );
    }

    #[tokio::test]
    async fn git_sha1_chunks() {
        let data = vec![&b"hello"[..], &b", "[..], &b"world"[..]] // 8c01d89ae06311834ee4b1fab2f0414d35f01102
            .into_iter()
            .map(Bytes::from);
        let s = stream::iter(data);

        let res: RichGitSha1 =
            hash_stream(GitSha1IncrementalHasher::new(ExpectedSize::new(12)), s).await;

        assert_eq!(
            res,
            RichGitSha1::from_bytes(
                [
                    0x8c, 0x01, 0xd8, 0x9a, 0xe0, 0x63, 0x11, 0x83, 0x4e, 0xe4, 0xb1, 0xfa, 0xb2,
                    0xf0, 0x41, 0x4d, 0x35, 0xf0, 0x11, 0x02
                ],
                "blob",
                12
            )
            .unwrap()
        );
    }

    #[tokio::test]
    async fn sha256_simple() {
        let data = Bytes::from(&b"hello, world"[..]); // 09ca7e4eaa6e8ae9c7d261167129184883644d07dfba7cbfbc4c8a2e08360d5b
        let s = stream::once(future::ready(data));

        let res: Sha256 = hash_stream(Sha256IncrementalHasher::new(), s).await;

        assert_eq!(
            res,
            Sha256::from_bytes([
                0x09, 0xca, 0x7e, 0x4e, 0xaa, 0x6e, 0x8a, 0xe9, 0xc7, 0xd2, 0x61, 0x16, 0x71, 0x29,
                0x18, 0x48, 0x83, 0x64, 0x4d, 0x07, 0xdf, 0xba, 0x7c, 0xbf, 0xbc, 0x4c, 0x8a, 0x2e,
                0x08, 0x36, 0x0d, 0x5b,
            ],)
            .unwrap()
        );
    }

    #[tokio::test]
    async fn sha256_chunks() {
        let data = vec![&b"hello"[..], &b", "[..], &b"world"[..]] // 09ca7e4eaa6e8ae9c7d261167129184883644d07dfba7cbfbc4c8a2e08360d5b
            .into_iter()
            .map(Bytes::from);
        let s = stream::iter(data);

        let res: Sha256 = hash_stream(Sha256IncrementalHasher::new(), s).await;

        assert_eq!(
            res,
            Sha256::from_bytes([
                0x09, 0xca, 0x7e, 0x4e, 0xaa, 0x6e, 0x8a, 0xe9, 0xc7, 0xd2, 0x61, 0x16, 0x71, 0x29,
                0x18, 0x48, 0x83, 0x64, 0x4d, 0x07, 0xdf, 0xba, 0x7c, 0xbf, 0xbc, 0x4c, 0x8a, 0x2e,
                0x08, 0x36, 0x0d, 0x5b,
            ],)
            .unwrap()
        );
    }

    #[tokio::test]
    async fn content_id_simple() {
        let data = Bytes::from(&b"hello, world"[..]); // 19d95f338fa0f547f773c12353eb6dacb1a7ce9b0402515484e8276055774b35
        let s = stream::once(future::ready(data));

        let res: ContentId = hash_stream(ContentIdIncrementalHasher::new(), s).await;

        assert_eq!(
            res,
            ContentId::from_bytes([
                0x19, 0xd9, 0x5f, 0x33, 0x8f, 0xa0, 0xf5, 0x47, 0xf7, 0x73, 0xc1, 0x23, 0x53, 0xeb,
                0x6d, 0xac, 0xb1, 0xa7, 0xce, 0x9b, 0x04, 0x02, 0x51, 0x54, 0x84, 0xe8, 0x27, 0x60,
                0x55, 0x77, 0x4b, 0x35,
            ],)
            .unwrap()
        );
    }

    #[tokio::test]
    async fn content_id_chunks() {
        let data = vec![&b"hello"[..], &b", "[..], &b"world"[..]] // 19d95f338fa0f547f773c12353eb6dacb1a7ce9b0402515484e8276055774b35
            .into_iter()
            .map(Bytes::from);
        let s = stream::iter(data);

        let res: ContentId = hash_stream(ContentIdIncrementalHasher::new(), s).await;

        assert_eq!(
            res,
            ContentId::from_bytes([
                0x19, 0xd9, 0x5f, 0x33, 0x8f, 0xa0, 0xf5, 0x47, 0xf7, 0x73, 0xc1, 0x23, 0x53, 0xeb,
                0x6d, 0xac, 0xb1, 0xa7, 0xce, 0x9b, 0x04, 0x02, 0x51, 0x54, 0x84, 0xe8, 0x27, 0x60,
                0x55, 0x77, 0x4b, 0x35,
            ],)
            .unwrap()
        );
    }

    #[tokio::test]
    async fn sha1_empty() {
        let s = stream::empty::<Vec<u8>>();

        let res: Sha1 = hash_stream(Sha1IncrementalHasher::new(), s).await;

        assert_eq!(
            res,
            Sha1::from_bytes([
                0xda, 0x39, 0xa3, 0xee, 0x5e, 0x6b, 0x4b, 0x0d, 0x32, 0x55, 0xbf, 0xef, 0x95, 0x60,
                0x18, 0x90, 0xaf, 0xd8, 0x07, 0x09
            ])
            .unwrap()
        );
    }

    #[tokio::test]
    async fn seeded_blake3_simple() {
        let data = Bytes::from(&b"hello, world"[..]); // 8537f0e61caaa8f1aca4eac89bb9f27ba2562dbdd1b1a2d987999c37f159bf18
        let s = stream::once(future::ready(data));

        let res: Blake3 = hash_stream(Blake3IncrementalHasher::new_seeded(), s).await;

        let hello_world_hash = Blake3::from_bytes([
            0x85, 0x37, 0xf0, 0xe6, 0x1c, 0xaa, 0xa8, 0xf1, 0xac, 0xa4, 0xea, 0xc8, 0x9b, 0xb9,
            0xf2, 0x7b, 0xa2, 0x56, 0x2d, 0xbd, 0xd1, 0xb1, 0xa2, 0xd9, 0x87, 0x99, 0x9c, 0x37,
            0xf1, 0x59, 0xbf, 0x18,
        ])
        .unwrap();
        assert_eq!(res, hello_world_hash);
    }

    #[tokio::test]
    async fn seeded_blake3_chunks() {
        let data = vec![&b"hello"[..], &b", "[..], &b"world"[..]] // 8537f0e61caaa8f1aca4eac89bb9f27ba2562dbdd1b1a2d987999c37f159bf18
            .into_iter()
            .map(Bytes::from);
        let s = stream::iter(data);

        let res: Blake3 = hash_stream(Blake3IncrementalHasher::new_seeded(), s).await;
        let hello_world_hash = Blake3::from_bytes([
            0x85, 0x37, 0xf0, 0xe6, 0x1c, 0xaa, 0xa8, 0xf1, 0xac, 0xa4, 0xea, 0xc8, 0x9b, 0xb9,
            0xf2, 0x7b, 0xa2, 0x56, 0x2d, 0xbd, 0xd1, 0xb1, 0xa2, 0xd9, 0x87, 0x99, 0x9c, 0x37,
            0xf1, 0x59, 0xbf, 0x18,
        ])
        .unwrap();
        assert_eq!(res, hello_world_hash);
    }
}
