//# run
script {
    const SHL0: u8 = 1 << 7;
    const SHL1: u64 = 1 << 63;
    const SHL2: u128 = 1 << 127;

    const SHR0: u8 = 128 >> 7;
    const SHR1: u64 = 18446744073709551615 >> 63;
    const SHR2: u128 = 340282366920938463463374607431768211455 >> 127;

    const DIV0: u8 = 255 / 2;
    const DIV1: u64 = 18446744073709551615 / 2;
    const DIV2: u128 = 340282366920938463463374607431768211455 / 2;

    const MOD0: u8 = 255 % 2;
    const MOD1: u64 = 18446744073709551615 % 2;
    const MOD2: u128 = 340282366920938463463374607431768211455 % 2;

    const ADD0: u8 = 254 + 1;
    const ADD1: u64 = 18446744073709551614 + 1;
    const ADD2: u128 = 340282366920938463463374607431768211454 + 1;

    const SUB0: u8 = 255 - 255;
    const SUB1: u64 = 18446744073709551615 - 18446744073709551615;
    const SUB2: u128 =
        340282366920938463463374607431768211455 - 340282366920938463463374607431768211455;

    const CAST0: u8 = ((255: u64) as u8);
    const CAST1: u64 = ((18446744073709551615: u128) as u64);
    const CAST2: u128 = ((1: u8) as u128);

    const BAND0: u8 = 255 & 255;
    const BAND1: u64 = 18446744073709551615 & 18446744073709551615;
    const BAND2: u128 =
        340282366920938463463374607431768211455 & 340282366920938463463374607431768211455;

    const BOR0: u8 = 255 | 255;
    const BOR1: u64 = 18446744073709551615 | 18446744073709551615;
    const BOR2: u128 =
        340282366920938463463374607431768211455 | 340282366920938463463374607431768211455;

    const BXOR0: u8 = 255 ^ 255;
    const BXOR1: u64 = 18446744073709551615 ^ 18446744073709551615;
    const BXOR2: u128 =
        340282366920938463463374607431768211455 ^ 340282366920938463463374607431768211455;

    fun main() {
        assert(SHL0 == 128, 42);
        assert(SHL1 == 9223372036854775808, 42);
        assert(SHL2 == 170141183460469231731687303715884105728, 42);
        assert(SHL0 == 0x80, 42);
        assert(SHL1 == 0x8000000000000000, 42);
        assert(SHL2 == 0x80000000000000000000000000000000, 42);

        assert(SHR0 == 1, 42);
        assert(SHR1 == 1, 42);
        assert(SHR2 == 1, 42);
        assert(SHR0 == 0x1, 42);
        assert(SHR1 == 0x1, 42);
        assert(SHR2 == 0x1, 42);


        assert(DIV0 == 127, 42);
        assert(DIV1 == 9223372036854775807, 42);
        assert(DIV2 == 170141183460469231731687303715884105727, 42);
        assert(DIV0 == 0x7F, 42);
        assert(DIV1 == 0x7FFFFFFFFFFFFFFF, 42);
        assert(DIV2 == 0x7FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF, 42);

        assert(MOD0 == 1, 42);
        assert(MOD1 == 1, 42);
        assert(MOD2 == 1, 42);
        assert(MOD0 == 0x1, 42);
        assert(MOD1 == 0x1, 42);
        assert(MOD2 == 0x1, 42);

        assert(ADD0 == 255, 42);
        assert(ADD1 == 18446744073709551615, 42);
        assert(ADD2 == 340282366920938463463374607431768211455, 42);
        assert(ADD0 == 0xFF, 42);
        assert(ADD1 == 0xFFFFFFFFFFFFFFFF, 42);
        assert(ADD2 == 0xFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF, 42);

        assert(SUB0 == 0, 42);
        assert(SUB1 == 0, 42);
        assert(SUB2 == 0, 42);
        assert(SUB0 == 0x0, 42);
        assert(SUB1 == 0x0, 42);
        assert(SUB2 == 0x0, 42);

        assert(CAST0 == 255, 42);
        assert(CAST1 == 18446744073709551615, 42);
        assert(CAST2 == 1, 42);
        assert(CAST0 == 0xFF, 42);
        assert(CAST1 == 0xFFFFFFFFFFFFFFFF, 42);
        assert(CAST2 == 0x1, 42);

        assert(BAND0 == 255, 42);
        assert(BAND1 == 18446744073709551615, 42);
        assert(BAND2 == 340282366920938463463374607431768211455, 42);
        assert(BAND0 == 0xFF, 42);
        assert(BAND1 == 0xFFFFFFFFFFFFFFFF, 42);
        assert(BAND2 == 0xFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF, 42);

        assert(BXOR0 == 0, 42);
        assert(BXOR1 == 0, 42);
        assert(BXOR2 == 0, 42);
        assert(BXOR0 == 0x0, 42);
        assert(BXOR1 == 0x0, 42);
        assert(BXOR2 == 0x0, 42);
    }
}
