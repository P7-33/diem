#[test_only]
module Std::EventTests {
    //////////////////
    // Storage tests
    //////////////////

    use Std::BCS;
    use Std::Event::{Self, EventHandle, emit_event, new_event_handle};
    use Std::Signer::address_of;
    use Std::Vector;

    struct Box<T> has copy, drop, store { x: T }
    struct Box3<T> has copy, drop, store { x: Box<Box<T>> }
    struct Box7<T> has copy, drop, store { x: Box3<Box3<T>> }
    struct Box15<T> has copy, drop, store { x: Box7<Box7<T>> }
    struct Box31<T> has copy, drop, store { x: Box15<Box15<T>> }
    struct Box63<T> has copy, drop, store { x: Box31<Box31<T>> }
    struct Box127<T> has copy, drop, store { x: Box63<Box63<T>> }
    struct Box255<T> has copy, drop, store { x: Box127<Box127<T>> }

    struct MyEvent<phantom T: copy + drop + store> has key {
        e: EventHandle<T>
    }

    fun box3<T>(x: T): Box3<T> {
        Box3 { x: Box { x: Box { x } } }
    }

    fun box7<T>(x: T): Box7<T> {
        Box7 { x: box3(box3(x)) }
    }

    fun box15<T>(x: T): Box15<T> {
        Box15 { x: box7(box7(x)) }
    }

    fun box31<T>(x: T): Box31<T> {
        Box31 { x: box15(box15(x)) }
    }

    fun box63<T>(x: T): Box63<T> {
        Box63 { x: box31(box31(x)) }
    }

    fun box127<T>(x: T): Box127<T> {
        Box127 { x: box63(box63(x)) }
    }

    fun box255<T>(x: T): Box255<T> {
        Box255 { x: box127(box127(x)) }
    }

    fun maybe_init_event<T: copy + drop + store>(s: &signer) {
        if (exists<MyEvent<T>>(address_of(s))) return;

        move_to(s, MyEvent { e: new_event_handle<T>(s)})
    }

    public fun event_128(s: &signer) acquires MyEvent {
        maybe_init_event<Box127<bool>>(s);

        emit_event(&mut borrow_global_mut<MyEvent<Box127<bool>>>(address_of(s)).e, box127(true))
    }

    public fun event_256(s: &signer) acquires MyEvent {
        maybe_init_event<Box255<bool>>(s);

        emit_event(&mut borrow_global_mut<MyEvent<Box255<bool>>>(address_of(s)).e, box255(true))
    }

    public fun event_257(s: &signer) acquires MyEvent {
        maybe_init_event<Box<Box255<bool>>>(s);

        // will abort
        emit_event(
            &mut borrow_global_mut<MyEvent<Box<Box255<bool>>>>(address_of(s)).e,
            Box { x: box255(true) }
        )
    }

    #[test(s = @0x42)]
    fun test_event_128(s: signer) acquires MyEvent {
        event_128(&s);
    }

    #[test(s = @0x42)]
    fun test_event_256(s: signer) acquires MyEvent {
        event_256(&s);
    }

    #[test(s = @0x42)]
    #[expected_failure(abort_code = 0)]
    fun test_event_257(s: signer) acquires MyEvent {
        event_257(&s);
    }

    // More detailed version of the above--test BCS compatibility between the old event
    // format and the new wrapper hack.
    // this test lives here because it is important for the correctness of GUIDWrapper;
    // see the comments there for more details
    #[test(s = @0x42)]
    fun test_guid_wrapper_backward_compatibility(s: signer) {
        let sender_bytes = BCS::to_bytes(&address_of(&s));
        let count_bytes = BCS::to_bytes(&0u64);
        Vector::append(&mut count_bytes, sender_bytes);
        let old_guid = count_bytes;
        // should be 16 bytes of address + 8 byte integer
        assert(Vector::length(&old_guid) == 24, 0);
        let old_guid_bytes = BCS::to_bytes(&old_guid);
        // old_guid_bytes should be length prefix (24), followed by content of vector
        // the length prefix is a ULEB encoded 32-bit value, so for length prefix 24,
        // this should only occupy 1 byte: https://github.com/diem/bcs#uleb128-encoded-integers
        // hence, 24 byte contents + 1 byte length prefix = 25 bytes
        assert(Vector::length(&old_guid_bytes) == 25, 1);

        // now, build a new GUID and check byte-for-byte compatibility
        let guid_wrapper = Event::create_guid_wrapper_for_test<u64>(&s);
        let guid_wrapper_bytes = BCS::to_bytes(&guid_wrapper);

        // check that the guid grapper bytes are identical to the old guid bytes
        assert(Vector::length(&guid_wrapper_bytes) == Vector::length(&old_guid_bytes), 2);
        assert(guid_wrapper_bytes == old_guid_bytes, 3)
    }
}
