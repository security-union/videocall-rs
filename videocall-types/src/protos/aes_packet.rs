/*
 * Copyright 2025 Security Union LLC
 *
 * Licensed under either of
 *
 * * Apache License, Version 2.0
 *   (http://www.apache.org/licenses/LICENSE-2.0)
 * * MIT license
 *   (http://opensource.org/licenses/MIT)
 *
 * at your option.
 *
 * Unless you explicitly state otherwise, any contribution intentionally
 * submitted for inclusion in the work by you, as defined in the Apache-2.0
 * license, shall be dual licensed as above, without any additional terms or
 * conditions.
 */



// This file is generated by rust-protobuf 3.7.1. Do not edit
// .proto file is parsed by protoc --rs_out=...
// @generated

// https://github.com/rust-lang/rust-clippy/issues/702
#![allow(unknown_lints)]
#![allow(clippy::all)]

#![allow(unused_attributes)]
#![cfg_attr(rustfmt, rustfmt::skip)]

#![allow(dead_code)]
#![allow(missing_docs)]
#![allow(non_camel_case_types)]
#![allow(non_snake_case)]
#![allow(non_upper_case_globals)]
#![allow(trivial_casts)]
#![allow(unused_results)]
#![allow(unused_mut)]

//! Generated file from `types/aes_packet.proto`

/// Generated files are compatible only with the same version
/// of protobuf runtime.
const _PROTOBUF_VERSION_CHECK: () = ::protobuf::VERSION_3_7_1;

// @@protoc_insertion_point(message:AesPacket)
#[derive(PartialEq,Clone,Default,Debug)]
pub struct AesPacket {
    // message fields
    // @@protoc_insertion_point(field:AesPacket.key)
    pub key: ::std::vec::Vec<u8>,
    // @@protoc_insertion_point(field:AesPacket.iv)
    pub iv: ::std::vec::Vec<u8>,
    // special fields
    // @@protoc_insertion_point(special_field:AesPacket.special_fields)
    pub special_fields: ::protobuf::SpecialFields,
}

impl<'a> ::std::default::Default for &'a AesPacket {
    fn default() -> &'a AesPacket {
        <AesPacket as ::protobuf::Message>::default_instance()
    }
}

impl AesPacket {
    pub fn new() -> AesPacket {
        ::std::default::Default::default()
    }

    fn generated_message_descriptor_data() -> ::protobuf::reflect::GeneratedMessageDescriptorData {
        let mut fields = ::std::vec::Vec::with_capacity(2);
        let mut oneofs = ::std::vec::Vec::with_capacity(0);
        fields.push(::protobuf::reflect::rt::v2::make_simpler_field_accessor::<_, _>(
            "key",
            |m: &AesPacket| { &m.key },
            |m: &mut AesPacket| { &mut m.key },
        ));
        fields.push(::protobuf::reflect::rt::v2::make_simpler_field_accessor::<_, _>(
            "iv",
            |m: &AesPacket| { &m.iv },
            |m: &mut AesPacket| { &mut m.iv },
        ));
        ::protobuf::reflect::GeneratedMessageDescriptorData::new_2::<AesPacket>(
            "AesPacket",
            fields,
            oneofs,
        )
    }
}

impl ::protobuf::Message for AesPacket {
    const NAME: &'static str = "AesPacket";

    fn is_initialized(&self) -> bool {
        true
    }

    fn merge_from(&mut self, is: &mut ::protobuf::CodedInputStream<'_>) -> ::protobuf::Result<()> {
        while let Some(tag) = is.read_raw_tag_or_eof()? {
            match tag {
                10 => {
                    self.key = is.read_bytes()?;
                },
                18 => {
                    self.iv = is.read_bytes()?;
                },
                tag => {
                    ::protobuf::rt::read_unknown_or_skip_group(tag, is, self.special_fields.mut_unknown_fields())?;
                },
            };
        }
        ::std::result::Result::Ok(())
    }

    // Compute sizes of nested messages
    #[allow(unused_variables)]
    fn compute_size(&self) -> u64 {
        let mut my_size = 0;
        if !self.key.is_empty() {
            my_size += ::protobuf::rt::bytes_size(1, &self.key);
        }
        if !self.iv.is_empty() {
            my_size += ::protobuf::rt::bytes_size(2, &self.iv);
        }
        my_size += ::protobuf::rt::unknown_fields_size(self.special_fields.unknown_fields());
        self.special_fields.cached_size().set(my_size as u32);
        my_size
    }

    fn write_to_with_cached_sizes(&self, os: &mut ::protobuf::CodedOutputStream<'_>) -> ::protobuf::Result<()> {
        if !self.key.is_empty() {
            os.write_bytes(1, &self.key)?;
        }
        if !self.iv.is_empty() {
            os.write_bytes(2, &self.iv)?;
        }
        os.write_unknown_fields(self.special_fields.unknown_fields())?;
        ::std::result::Result::Ok(())
    }

    fn special_fields(&self) -> &::protobuf::SpecialFields {
        &self.special_fields
    }

    fn mut_special_fields(&mut self) -> &mut ::protobuf::SpecialFields {
        &mut self.special_fields
    }

    fn new() -> AesPacket {
        AesPacket::new()
    }

    fn clear(&mut self) {
        self.key.clear();
        self.iv.clear();
        self.special_fields.clear();
    }

    fn default_instance() -> &'static AesPacket {
        static instance: AesPacket = AesPacket {
            key: ::std::vec::Vec::new(),
            iv: ::std::vec::Vec::new(),
            special_fields: ::protobuf::SpecialFields::new(),
        };
        &instance
    }
}

impl ::protobuf::MessageFull for AesPacket {
    fn descriptor() -> ::protobuf::reflect::MessageDescriptor {
        static descriptor: ::protobuf::rt::Lazy<::protobuf::reflect::MessageDescriptor> = ::protobuf::rt::Lazy::new();
        descriptor.get(|| file_descriptor().message_by_package_relative_name("AesPacket").unwrap()).clone()
    }
}

impl ::std::fmt::Display for AesPacket {
    fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
        ::protobuf::text_format::fmt(self, f)
    }
}

impl ::protobuf::reflect::ProtobufValue for AesPacket {
    type RuntimeType = ::protobuf::reflect::rt::RuntimeTypeMessage<Self>;
}

static file_descriptor_proto_data: &'static [u8] = b"\
    \n\x16types/aes_packet.proto\"-\n\tAesPacket\x12\x10\n\x03key\x18\x01\
    \x20\x01(\x0cR\x03key\x12\x0e\n\x02iv\x18\x02\x20\x01(\x0cR\x02ivJ\x98\
    \x01\n\x06\x12\x04\0\0\x05\x01\n\x08\n\x01\x0c\x12\x03\0\0\x12\n\n\n\x02\
    \x04\0\x12\x04\x02\0\x05\x01\n\n\n\x03\x04\0\x01\x12\x03\x02\x08\x11\n\
    \x0b\n\x04\x04\0\x02\0\x12\x03\x03\x02\x10\n\x0c\n\x05\x04\0\x02\0\x05\
    \x12\x03\x03\x02\x07\n\x0c\n\x05\x04\0\x02\0\x01\x12\x03\x03\x08\x0b\n\
    \x0c\n\x05\x04\0\x02\0\x03\x12\x03\x03\x0e\x0f\n\x0b\n\x04\x04\0\x02\x01\
    \x12\x03\x04\x02\x0f\n\x0c\n\x05\x04\0\x02\x01\x05\x12\x03\x04\x02\x07\n\
    \x0c\n\x05\x04\0\x02\x01\x01\x12\x03\x04\x08\n\n\x0c\n\x05\x04\0\x02\x01\
    \x03\x12\x03\x04\r\x0eb\x06proto3\
";

/// `FileDescriptorProto` object which was a source for this generated file
fn file_descriptor_proto() -> &'static ::protobuf::descriptor::FileDescriptorProto {
    static file_descriptor_proto_lazy: ::protobuf::rt::Lazy<::protobuf::descriptor::FileDescriptorProto> = ::protobuf::rt::Lazy::new();
    file_descriptor_proto_lazy.get(|| {
        ::protobuf::Message::parse_from_bytes(file_descriptor_proto_data).unwrap()
    })
}

/// `FileDescriptor` object which allows dynamic access to files
pub fn file_descriptor() -> &'static ::protobuf::reflect::FileDescriptor {
    static generated_file_descriptor_lazy: ::protobuf::rt::Lazy<::protobuf::reflect::GeneratedFileDescriptor> = ::protobuf::rt::Lazy::new();
    static file_descriptor: ::protobuf::rt::Lazy<::protobuf::reflect::FileDescriptor> = ::protobuf::rt::Lazy::new();
    file_descriptor.get(|| {
        let generated_file_descriptor = generated_file_descriptor_lazy.get(|| {
            let mut deps = ::std::vec::Vec::with_capacity(0);
            let mut messages = ::std::vec::Vec::with_capacity(1);
            messages.push(AesPacket::generated_message_descriptor_data());
            let mut enums = ::std::vec::Vec::with_capacity(0);
            ::protobuf::reflect::GeneratedFileDescriptor::new_generated(
                file_descriptor_proto(),
                deps,
                messages,
                enums,
            )
        });
        ::protobuf::reflect::FileDescriptor::new_generated_2(generated_file_descriptor)
    })
}
