use crate::vl1::node::PacketBuffer;
use crate::vl1::constants::FRAGMENT_COUNT_MAX;

/// Packet fragment re-assembler and container.
/// This is only used in the receive path.
pub(crate) struct FragmentedPacket {
    pub ts_ticks: i64,
    pub frags: [Option<PacketBuffer>; FRAGMENT_COUNT_MAX],
    pub have: u8,
    pub expecting: u8,
}

impl FragmentedPacket {
    /// Add a fragment to this fragment set and return true if the packet appears complete.
    /// This will panic if 'no' is out of bounds.
    #[inline(always)]
    pub fn add_fragment(&mut self, frag: PacketBuffer, no: u8, expecting: u8) -> bool {
        if no < FRAGMENT_COUNT_MAX as u8 {
            if self.frags[no as usize].replace(frag).is_none() {
                self.have = self.have.wrapping_add(1);
                self.expecting |= expecting; // in valid streams expecting is either 0 or the (same) total
                return self.have == self.expecting;
            }
        }
        false
    }
}
