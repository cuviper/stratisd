use std::{
    self,
    cmp::Ordering,
    io::{Read, Seek, SeekFrom},
};

use byteorder::{ByteOrder, LittleEndian};
use chrono::{DateTime, TimeZone, Utc};
use crc::crc32;

use devicemapper::Bytes;

use crate::{
    engine::strat_engine::device::SyncAll,
    stratis::{ErrorEnum, StratisError, StratisResult},
};

pub use self::mda_size::{MDADataSize, MDARegionSize, MDASize, MIN_MDA_DATA_REGION_SIZE};

const STRAT_REGION_HDR_VERSION: u8 = 1;
const STRAT_METADATA_VERSION: u8 = 1;

/// A module which defines types for three different regions of the MDA:
/// * MDADataSize: the size of the region for variable length metadata
/// * MDARegionSize: the size a single MDA region
/// * MDASize: the size of the whole MDA
mod mda_size {

    use devicemapper::{Bytes, Sectors};

    pub const _MDA_REGION_HDR_SIZE: usize = 32;
    const MDA_REGION_HDR_SIZE: Bytes = Bytes(_MDA_REGION_HDR_SIZE as u64);

    // The minimum size allocated for variable length metadata
    pub const MIN_MDA_DATA_REGION_SIZE: Bytes = Bytes(260_064);

    pub const NUM_PRIMARY_MDA_REGIONS: usize = 2;

    // There are two copies of every primary MDA region, so the total number
    // of MDA regions is twice the number of primary MDA regions.
    pub const NUM_MDA_REGIONS: usize = 2 * NUM_PRIMARY_MDA_REGIONS;

    /// A value representing the size of the entire MDA.
    /// It is constructed in one of two ways:
    /// * By reading a value from a device in constructing a StaticHeader
    /// * MDARegionSize::mda_size
    /// Since only a valid MDASize can be constructed, only a valid MDASize
    /// can be written. An error on reading ought to be detected by
    /// checksums.
    /// Since MDARegionSize is always at least the minimum, the result of
    /// MDARegionSize::mda_size is at least the minimum. The method, by
    /// definition, constructs a valid MDASize value.
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub struct MDASize(pub Sectors);

    impl Default for MDASize {
        fn default() -> MDASize {
            MDARegionSize::default().mda_size()
        }
    }

    impl MDASize {
        pub fn sectors(self) -> Sectors {
            self.0
        }

        pub fn region_size(self) -> MDARegionSize {
            MDARegionSize(self.0 / NUM_MDA_REGIONS)
        }
    }

    /// A value representing the size of one MDA region.
    /// Values of this type are created by one of two methods:
    /// * MDASize::region_size
    /// * MDADataSize::region_size
    /// Since an MDADataSize is always at least the minimum required by the
    /// design specification, MDADataSize::region_size() always yields a
    /// value of at least the minimum required size.
    /// Since an MDASize is always valid, and at least the minimum,
    /// MDASize::region_size() always yields a valid and sufficiently large
    /// region.
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub struct MDARegionSize(pub Sectors);

    impl Default for MDARegionSize {
        fn default() -> MDARegionSize {
            MDADataSize::default().region_size()
        }
    }

    impl MDARegionSize {
        pub fn sectors(self) -> Sectors {
            self.0
        }

        pub fn mda_size(self) -> MDASize {
            MDASize(self.0 * NUM_MDA_REGIONS)
        }

        pub fn data_size(self) -> MDADataSize {
            MDADataSize(self.0.bytes() - MDA_REGION_HDR_SIZE)
        }
    }

    /// A type representing the size of the region for storing variable length
    /// metadata. A newly created value is never less than the minimum required
    /// by the design specification.
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub struct MDADataSize(Bytes);

    impl Default for MDADataSize {
        fn default() -> MDADataSize {
            MDADataSize(MIN_MDA_DATA_REGION_SIZE)
        }
    }

    impl MDADataSize {
        /// Create a new value, bounded from below by the minimum allowed.
        // Note that this code is dead due to GitHub issue:
        // https://github.com/stratis-storage/stratisd/issues/754.
        // To fix that bug it is necessary for client code to specify a
        // size. It will use this method to do that.
        #[allow(dead_code)]
        pub fn new(value: Bytes) -> MDADataSize {
            if value > MIN_MDA_DATA_REGION_SIZE {
                MDADataSize(value)
            } else {
                MDADataSize::default()
            }
        }

        pub fn region_size(self) -> MDARegionSize {
            let bytes = self.0 + MDA_REGION_HDR_SIZE;
            let sectors = bytes.sectors();
            MDARegionSize(if sectors.bytes() != bytes {
                sectors + Sectors(1)
            } else {
                sectors
            })
        }

        pub fn bytes(self) -> Bytes {
            self.0
        }
    }
}

/// Manages the MDA regions which hold the variable length metadata.
#[derive(Debug)]
pub struct MDARegions {
    /// The size of a single MDA region. The MDAHeader occupies the
    /// first few bytes of its region, the rest is available for the
    /// variable length metadata.
    region_size: MDARegionSize,
    /// The MDA headers which contain information about the variable
    /// length metadata. NUM_PRIMARY_MDA_REGIONS is 2: in the general
    /// case one is more recently written than the other.
    mdas: [Option<MDAHeader>; mda_size::NUM_PRIMARY_MDA_REGIONS],
}

impl MDARegions {
    /// Calculate the offset from start of device for an MDARegion.
    fn mda_offset(header_size: Bytes, index: usize, per_region_size: Bytes) -> u64 {
        *(header_size + per_region_size * index)
    }

    /// The maximum size of variable length metadata that this region
    /// can accommodate.
    pub fn max_data_size(&self) -> MDADataSize {
        self.region_size.data_size()
    }

    /// Initialize the space allotted to the MDA regions to 0.
    /// Return an MDARegions object with uninitialized MDAHeader objects.
    pub fn initialize<F>(
        header_size: Bytes,
        mda_size: MDASize,
        f: &mut F,
    ) -> StratisResult<MDARegions>
    where
        F: Seek + SyncAll,
    {
        let hdr_buf = MDAHeader::default().to_buf();

        let region_size = mda_size.region_size();
        let region_size_bytes = region_size.sectors().bytes();
        for region in 0..mda_size::NUM_MDA_REGIONS {
            f.seek(SeekFrom::Start(MDARegions::mda_offset(
                header_size,
                region,
                region_size_bytes,
            )))?;
            f.write_all(&hdr_buf)?;
        }

        f.sync_all()?;

        Ok(MDARegions {
            region_size,
            mdas: [None, None],
        })
    }

    /// Construct MDARegions from data on the disk.
    /// Note that this method is always called in a context where a
    /// StaticHeader has already been read. Therefore, it
    /// constitutes an error if it is not possible to discover two
    /// well-formed MDAHeaders for this device.
    pub fn load<F>(header_size: Bytes, mda_size: MDASize, f: &mut F) -> StratisResult<MDARegions>
    where
        F: Read + Seek,
    {
        let region_size = mda_size.region_size();
        let region_size_bytes = region_size.sectors().bytes();

        // Load a single region at the location specified by index.
        // If it appears that no metadata has been written at the location
        // return None. If it appears that there is metadata, but it has
        // been corrupted, return an error.
        let mut load_a_region = |index: usize| -> StratisResult<Option<MDAHeader>> {
            let mut hdr_buf = [0u8; mda_size::_MDA_REGION_HDR_SIZE];
            f.seek(SeekFrom::Start(MDARegions::mda_offset(
                header_size,
                index,
                region_size_bytes,
            )))?;
            f.read_exact(&mut hdr_buf)?;
            Ok(MDAHeader::from_buf(&hdr_buf)?)
        };

        // Get an MDAHeader for the given index.
        // If there is a failure reading the first, fall back on the
        // second. If there is a failure reading both, return an error.
        let mut get_mda = |index: usize| -> StratisResult<Option<MDAHeader>> {
            load_a_region(index)
                .or_else(|_| load_a_region(index + mda_size::NUM_PRIMARY_MDA_REGIONS))
        };

        Ok(MDARegions {
            region_size,
            mdas: [get_mda(0)?, get_mda(1)?],
        })
    }

    /// Write metadata to the older of the metadata regions.
    /// If operation is completed, update the value of the
    /// older MDAHeader with the new values.
    /// If time specified is earlier than the last update time, return an
    /// error. If the size of the data is greater than the available space,
    /// return an error. If there is an error when writing the data, return
    /// an error.
    pub fn save_state<F>(
        &mut self,
        header_size: Bytes,
        time: &DateTime<Utc>,
        data: &[u8],
        f: &mut F,
    ) -> StratisResult<()>
    where
        F: Seek + SyncAll,
    {
        if self.last_update_time() >= Some(time) {
            return Err(StratisError::Engine(
                ErrorEnum::Invalid,
                "Overwriting newer data".into(),
            ));
        }

        let used = Bytes(data.len() as u64);
        let max_available = self.max_data_size().bytes();
        if used > max_available {
            let err_msg = format!(
                "metadata length {} exceeds region available {}",
                used, max_available
            );
            return Err(StratisError::Engine(ErrorEnum::Invalid, err_msg));
        };

        let header = MDAHeader {
            last_updated: *time,
            used,
            data_crc: crc32::checksum_castagnoli(data),
        };
        let hdr_buf = header.to_buf();

        // Write data to a region specified by index.
        let region_size = self.region_size.sectors().bytes();
        let mut save_region = |index: usize| -> StratisResult<()> {
            f.seek(SeekFrom::Start(MDARegions::mda_offset(
                header_size,
                index,
                region_size,
            )))?;
            f.write_all(&hdr_buf)?;
            f.write_all(data)?;
            f.sync_all()?;

            Ok(())
        };

        // TODO: Consider if there is an action that should be taken if
        // saving to one or the other region fails.
        let older_region = self.older();
        save_region(older_region)?;
        save_region(older_region + mda_size::NUM_PRIMARY_MDA_REGIONS)?;

        self.mdas[older_region] = Some(header);

        Ok(())
    }

    /// Load metadata from the newer MDA region.
    /// In case there is no record of metadata in regions, return None.
    /// If there is a record of metadata, and there is a failure to read
    /// the metadata, return an error.
    pub fn load_state<F>(&self, header_size: Bytes, f: &mut F) -> StratisResult<Option<Vec<u8>>>
    where
        F: Read + Seek,
    {
        let newer_region = self.newer();
        let mda = match self.mdas[newer_region] {
            None => return Ok(None),
            Some(ref mda) => mda,
        };
        let region_size = self.region_size.sectors().bytes();

        // Load the metadata region specified by index.
        // It is an error if the metadata can not be found.
        let mut load_region = |index: usize| -> StratisResult<Vec<u8>> {
            let offset = MDARegions::mda_offset(header_size, index, region_size)
                + mda_size::_MDA_REGION_HDR_SIZE as u64;
            f.seek(SeekFrom::Start(offset))?;
            mda.load_region(f)
        };

        // TODO: Figure out if there is an action to take if the
        // first read returns an error.
        load_region(newer_region)
            .or_else(|_| load_region(newer_region + mda_size::NUM_PRIMARY_MDA_REGIONS))
            .map(Some)
    }

    /// The index of the older region, or 0 if there is a tie.
    fn older(&self) -> usize {
        match (&self.mdas[0], &self.mdas[1]) {
            (&None, _) => 0,
            (_, &None) => 1,
            (&Some(ref mda0), &Some(ref mda1)) => match mda0.last_updated.cmp(&mda1.last_updated) {
                Ordering::Less => 0,
                Ordering::Equal | Ordering::Greater => 1,
            },
        }
    }

    /// The index of the newer region, or 1 if there is a tie.
    fn newer(&self) -> usize {
        match self.older() {
            0 => 1,
            1 => 0,
            _ => panic!("invalid val from older()"),
        }
    }

    /// The last update time for these MDA regions
    pub fn last_update_time(&self) -> Option<&DateTime<Utc>> {
        self.mdas[self.newer()].as_ref().map(|h| &h.last_updated)
    }
}

#[derive(Debug)]
pub struct MDAHeader {
    last_updated: DateTime<Utc>,

    /// Size of region used for pool metadata.
    used: Bytes,

    data_crc: u32,
}

// Implementing Default explicitly because DateTime<Utc> does not implement
// Default.
impl Default for MDAHeader {
    fn default() -> MDAHeader {
        MDAHeader {
            last_updated: Utc.timestamp(0, 0),
            used: Bytes(0),
            data_crc: 0,
        }
    }
}

impl MDAHeader {
    /// Parse a valid MDAHeader from buf.
    /// If the timestamp value is 0, then return None. This means that
    /// No variable length metadata has been written, so there is no
    /// corresponding header.
    fn parse_buf(buf: &[u8; mda_size::_MDA_REGION_HDR_SIZE]) -> Option<MDAHeader> {
        match LittleEndian::read_u64(&buf[16..24]) {
            0 => None,
            secs => {
                // Signed cast is safe, highest order bit of each value
                // read is guaranteed to be 0.
                assert!(secs <= std::i64::MAX as u64);

                Some(MDAHeader {
                    used: Bytes(LittleEndian::read_u64(&buf[8..16])),
                    last_updated: Utc.timestamp(secs as i64, LittleEndian::read_u32(&buf[24..28])),
                    data_crc: LittleEndian::read_u32(&buf[4..8]),
                })
            }
        }
    }

    /// Get an MDAHeader from the buffer.
    /// Return an error for a bad checksum.
    /// Return None if there is no MDAHeader to be read. This is detected if the
    /// timestamp region in the buffer is 0.
    fn from_buf(buf: &[u8; mda_size::_MDA_REGION_HDR_SIZE]) -> StratisResult<Option<MDAHeader>> {
        if LittleEndian::read_u32(&buf[..4]) != crc32::checksum_castagnoli(&buf[4..]) {
            return Err(StratisError::Engine(
                ErrorEnum::Invalid,
                "MDA region header CRC".into(),
            ));
        }

        // Even though hdr_version is positioned later in struct, check it
        // right after the CRC
        let hdr_version = buf[28];
        if hdr_version != STRAT_REGION_HDR_VERSION {
            return Err(StratisError::Engine(
                ErrorEnum::Invalid,
                format!("Unknown region header version: {}", hdr_version),
            ));
        }

        let metadata_version = buf[29];
        if metadata_version != STRAT_METADATA_VERSION {
            return Err(StratisError::Engine(
                ErrorEnum::Invalid,
                format!("Unknown metadata version: {}", metadata_version),
            ));
        }

        Ok(MDAHeader::parse_buf(buf))
    }

    fn to_buf(&self) -> [u8; mda_size::_MDA_REGION_HDR_SIZE] {
        // Unsigned casts are always safe, as sec and nsec values are never negative
        assert!(self.last_updated.timestamp() >= 0);

        let mut buf = [0u8; mda_size::_MDA_REGION_HDR_SIZE];

        LittleEndian::write_u32(&mut buf[4..8], self.data_crc);
        LittleEndian::write_u64(&mut buf[8..16], *self.used as u64);
        LittleEndian::write_u64(&mut buf[16..24], self.last_updated.timestamp() as u64);
        LittleEndian::write_u32(&mut buf[24..28], self.last_updated.timestamp_subsec_nanos());
        buf[28] = STRAT_REGION_HDR_VERSION;
        buf[29] = STRAT_METADATA_VERSION;

        let buf_crc = crc32::checksum_castagnoli(&buf[4..mda_size::_MDA_REGION_HDR_SIZE]);
        LittleEndian::write_u32(&mut buf[..4], buf_crc);

        buf
    }

    /// Given a pre-seek()ed File, load the MDA region and return the contents.
    /// Return an error if the data can not be read, since the existence
    /// of the MDAHeader implies that the data must be available.
    // MDAHeader cannot seek because it doesn't know which region it's in
    fn load_region<F>(&self, f: &mut F) -> StratisResult<Vec<u8>>
    where
        F: Read,
    {
        // This cast could fail if running on a 32-bit machine and
        // size of metadata is greater than 2^32 - 1 bytes, which is
        // unlikely.
        //
        // This comparison seems absurd when compiled in an environment
        // where usize is u64, which is usual. It is not absurd when
        // compiled in an environment where usize is u32.
        #![allow(clippy::absurd_extreme_comparisons)]
        assert!(*self.used <= std::usize::MAX as u64);
        let mut data_buf = vec![0u8; *self.used as usize];

        f.read_exact(&mut data_buf)?;

        if self.data_crc != crc32::checksum_castagnoli(&data_buf) {
            return Err(StratisError::Engine(
                ErrorEnum::Invalid,
                "MDA region data CRC".into(),
            ));
        }

        Ok(data_buf)
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use chrono::Utc;
    use proptest::{
        collection::{self, SizeRange},
        num,
    };

    use super::*;

    // 82102984128000 in decimal, approx 17 million years
    const UTC_TIMESTAMP_SECS_BOUND: i64 = 0x777_9beb_9f00;
    const UTC_TIMESTAMP_NSECS_BOUND: u32 = 2_000_000_000u32;

    #[test]
    /// Verify that default MDAHeader is all 0s except for CRC and versions.
    fn test_default_mda_header() {
        let buf = MDAHeader::default().to_buf();

        // First 4 bytes is CRC. Then:
        assert!(buf[4..28].iter().all(|x| *x == 0u8));
        assert_eq!(buf[28], STRAT_REGION_HDR_VERSION);
        assert_eq!(buf[29], STRAT_METADATA_VERSION);
        assert!(buf[30..].iter().all(|x| *x == 0u8));
    }

    #[test]
    /// Verify that loading the MDARegions fails if the regions are all 0s.
    /// Verify that loading MDARegions succeeds if the regions are properly
    /// initialized.
    fn test_reading_mda_regions() {
        let offset = Bytes(100);
        let buf_length = *(offset + MDASize::default().sectors().bytes()) as usize;
        let mut buf = Cursor::new(vec![0; buf_length]);
        assert_matches!(
            MDARegions::load(offset, MDASize::default(), &mut buf),
            Err(_)
        );

        MDARegions::initialize(offset, MDASize::default(), &mut buf).unwrap();
        let regions = MDARegions::load(offset, MDASize::default(), &mut buf).unwrap();
        assert_matches!(regions.last_update_time(), None);
    }

    proptest! {
        #[test]
        /// Using an arbitrary data buffer, construct an mda header buffer
        /// Read the mda header buffer twice.
        /// Verify that the resulting MDAHeaders have all equal components.
        /// Verify timestamp and data CRC against original values.
        fn mda_header(ref data in collection::vec(num::u8::ANY, SizeRange::default()),
                      // sec < 0: unwritable timestamp
                      // sec == 0: value of 0 is interpreted as no timestamp when read
                      sec in 1..UTC_TIMESTAMP_SECS_BOUND,
                      nsec in 0..UTC_TIMESTAMP_NSECS_BOUND) {

            let header = MDAHeader {
                last_updated: Utc.timestamp(sec, nsec),
                used: Bytes(data.len() as u64),
                data_crc: crc32::checksum_castagnoli(&data),
            };
            let buf = header.to_buf();
            let mda1 = MDAHeader::from_buf(&buf).unwrap().unwrap();
            let mda2 = MDAHeader::from_buf(&buf).unwrap().unwrap();

            prop_assert_eq!(mda1.last_updated, mda2.last_updated);
            prop_assert_eq!(mda1.used, mda2.used);
            prop_assert_eq!(mda1.data_crc, mda2.data_crc);
            prop_assert_eq!(header.last_updated, mda1.last_updated);
            prop_assert_eq!(header.data_crc, mda1.data_crc);
        }
    }

    /// Verify that bad crc causes an error.
    #[test]
    fn test_from_buf_crc_error() {
        let data = [0u8; 3];
        let header = MDAHeader {
            last_updated: Utc::now(),
            used: Bytes(data.len() as u64),
            data_crc: crc32::checksum_castagnoli(&data),
        };
        let mut buf = header.to_buf();
        LittleEndian::write_u32(&mut buf[..4], 0u32);
        assert_matches!(MDAHeader::from_buf(&buf), Err(_));
    }
}
