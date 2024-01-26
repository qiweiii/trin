// This number was chosen after some experimentation with different batch sizes.
// It was found that a batch size of 128 was the best compromise between speed and
// successful response rate. This number may change in the future.
pub const BATCH_SIZE: u64 = 128;

/// History * content key & value
pub const HEADER_WITH_PROOF_CONTENT_KEY: &str =
    "0x006251d65b8a8668efabe2f89c96a5b6332d83b3bbe585089ea6b2ab9b6754f5e9";
pub const HEADER_WITH_PROOF_CONTENT_VALUE: &str =
    "0x0800000023020000f90218a00409be8253ad6ac0eb2056bc94194c6ccb83c74f4292c40c82e2dc8203bdc759a01dcc4de8dec75d7aab85b567b6ccd41ad312451b948a7413f0a142fd40d49347942a65aca4d5fc5b5c859090a6c34d164135398226a0afbf9bfd23008e8df44a83bb51ade45b993b3253fbce69cf7cec5d628eca6d45a0a7120e4bd136c0b6bdb0fa4990649f8c34d10d180dbd5ad6d03502ae92d32308a0d78aa953fedc7f7c112b2686d0b2b7e37eba716dd1f5d74ef3c8a37005f35215b9010000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000008000000000000000000000000000000000000000000000000000000004000000000000000000040000000000000000000000000000000001000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000400000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000400000000000000000860b69dd9d66ce830f424a832fefd88303a68c8456bfb4e398d783010303844765746887676f312e352e31856c696e7578a0e962efb883f91286e4fc6fd12989a70f24c174bd087f472528137c4134af0a1a88e857c5acc15dd82701cead98e305c70563000000000000000000000000000000000000000000000000be1b4a7a57f5316eea09c5e3e349141c46c1cb43664a815d28644cd74f282ca122360456d89447c0d586a8f5490922ea86b20e056879d64d87d104c14c0e594a6d800f67f5331ee2e511dc20e169c644b3df0f4c6b7c1717fc29d4844050b74044b506bf91edd14825aaec4f36fc5ad97b9eed9773aa2df15f80dff21eb668e24d61c29c3fda0fb425078a0479c5ea375ff95ad7780d0cdc87012009fd4a3dd003b06c7a28d6188e6be50ac544548cc7e3ee6cd07a8129f5c6d4d494b62ee8d96d26d0875bc87b56be0bf3e45846c0e3773abfccc239fdab29640b4e2aef297efcc6cb89b00a2566221cb4197ece3f66c24ea89969bd16265a74910aaf08d775116191117416b8799d0984f452a6fba19623442a7f199ef1627f1ae7295963a67db5534a292f98edbfb419ed85756abe76cd2d2bff8eb9b848b1e7b80b8274bbc469a36dce58b48ae57be6312bca843463ac45c54122a9f3fa9dca124b0fd50bce300708549c77b81b031278b9d193464f5e4b14769f6018055a457a577c508e811bcf55b297df3509f3db7e66ec68451e25acfbf935200e246f71e3c48240d00020000000000000000000000000000000000000000000000000000000000000";

// Beacon chain mainnet genesis time: Tue Dec 01 2020 12:00:23 GMT+0000
pub const BEACON_GENESIS_TIME: u64 = 1606824023;
