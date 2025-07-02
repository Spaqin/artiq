from artiq.coredevice.rtio import rtio_output
from artiq.experiment import *
from artiq.coredevice import spi2
from artiq.language.core import kernel, delay
from artiq.language.units import us

class DDS:
    """LTC2000 DDS with polynomial control of amplitude and frequency.

    This driver controls a DDS core that can generate signals with polynomially-varying
    amplitude and frequency. This is useful for creating complex pulse shapes and
    chirps.

    The amplitude is controlled by a cubic spline, and the phase/frequency by a
    quadratic spline.

    :param channel: RTIO channel number of this DDS interface.
    :param core_device: Core device name.
    """
    kernel_invariants = {"core", "channel", "target_o"}

    def __init__(self, dmgr, channel, core_device="core"):
        self.core = dmgr.get(core_device)
        self.channel = channel
        self.target_o = channel << 8

    @kernel
    def set_waveform(self, b0: TInt32, b1: TInt32, b2: TInt64, b3: TInt64,
            c0: TInt32, c1: TInt32, c2: TInt32, shift: TInt32 = 0):
        """Set the DDS polynomial coefficients and update rate.

        The amplitude and frequency evolve over time. The hardware implements this
        using a forward difference method for polynomial evaluation. The parameters
        correspond to the initial values of the state registers.

        The `shift` parameter controls the update rate of the polynomial evaluation.
        The update period is `(1 << shift)` system clock cycles. This allows the same
        coefficient set to generate waveforms with vastly different time scales.

        The waveform is not updated to the DDS core until triggered.
        See :class:`Trigger` for the update triggering mechanism.

        :param b0: Initial amplitude (16-bit).
        :param b1: Initial amplitude slope (1st derivative, 32-bit).
        :param b2: Initial amplitude 2nd derivative (48-bit).
        :param b3: Initial amplitude 3rd derivative (48-bit).
        :param c0: Phase offset (18-bit).
        :param c1: Initial frequency tuning word (FTW) (32-bit).
        :param c2: Frequency chirp rate (32-bit).
        :param shift: Update rate divider (0-15). An update occurs every
            ``2**shift`` clock cycles. Defaults to 0 (update every cycle).
        """

        if not 0 <= shift <= 15:
            raise ValueError("Shift must be between 0 and 15")

        phase_msb = (c0 >> 2) & 0xFFFF   # Upper 16 bits of 18-bit phase value
        phase_lsb = c0 & 0x3             # Bottom 2 bits of 18-bit phase value

        coef_words = [
            b0 & 0xFFFF,                          # Word 0: amplitude offset
            b1 & 0xFFFF,                          # Word 1: damp low
            (b1 >> 16) & 0xFFFF,                  # Word 2: damp high
            b2 & 0xFFFF,                          # Word 3: ddamp low
            (b2 >> 16) & 0xFFFF,                  # Word 4: ddamp mid
            (b2 >> 32) & 0xFFFF,                  # Word 5: ddamp high
            b3 & 0xFFFF,                          # Word 6: dddamp low
            (b3 >> 16) & 0xFFFF,                  # Word 7: dddamp mid
            (b3 >> 32) & 0xFFFF,                  # Word 8: dddamp high

            phase_msb,                            # Word 9: phase offset main (16 bits)
            c1 & 0xFFFF,                          # Word 10: ftw low
            (c1 >> 16) & 0xFFFF,                  # Word 11: ftw high
            c2 & 0xFFFF,                          # Word 12: chirp low
            (c2 >> 16) & 0xFFFF,                  # Word 13: chirp high

            shift | (phase_lsb << 4),             # Word 14: shift[3:0] + phase_lsb[5:4] + reserved[15:6]
        ]

        for i in range(len(coef_words)):
            rtio_output(self.target_o | i, coef_words[i])
            delay_mu(int64(self.core.ref_multiplier))


class Trigger:
    """LTC2000 DDS coefficient update trigger.

    :param channel: RTIO channel number of the trigger interface.
    :param core_device: Core device name.
    """
    kernel_invariants = {"core", "channel", "target_o"}

    def __init__(self, dmgr, channel, core_device="core"):
        self.core = dmgr.get(core_device)
        self.channel = channel
        self.target_o = channel << 8

    @kernel
    def trigger(self, trig_out):
        """Triggers coefficient update of LTC2000 DDS channel(s).

        Each bit of `trig_out` corresponds to a DDS core. Setting a bit
        commits the pending coefficient update (from :meth:`DDS.set_waveform`)
        to the corresponding DDS core synchronously.

        :param trig_out: Coefficient update trigger bits.
        """
        rtio_output(self.target_o, trig_out)

class Clear:
    """LTC2000 DDS clear signal.

    :param channel: RTIO channel number of the clear interface.
    :param core_device: Core device name.
    """
    kernel_invariants = {"core", "channel", "target_o"}

    def __init__(self, dmgr, channel, core_device="core"):
        self.core = dmgr.get(core_device)
        self.channel = channel
        self.target_o = channel << 8

    @kernel
    def clear(self, clear_out):
        """Clears the LTC2000 DDS channel(s).

        Each bit of `clear_out` corresponds to a DDS core. Setting a bit
        clears the internal state of the corresponding DDS core synchronously.

        :param clear_out: Clear signal bits.
        """
        rtio_output(self.target_o, clear_out)

class Reset:
    """LTC2000 DAC reset signal.

    :param channel: RTIO channel number of the reset interface.
    :param core_device: Core device name.
    """
    kernel_invariants = {"core", "channel", "target_o"}

    def __init__(self, dmgr, channel, core_device="core"):
        self.core = dmgr.get(core_device)
        self.channel = channel
        self.target_o = channel << 8

    @kernel
    def reset(self, reset):
        """Resets the LTC2000 DAC.

        :param reset: Reset signal.
        """
        rtio_output(self.target_o, reset)

class Gain:
    """LTC2000 sub DDS gain control.

    Not yet fully implemented.
    """
    kernel_invariants = {"core", "channel", "target_o"}

    def __init__(self, dmgr, channel, core_device="core"):
        self.core = dmgr.get(core_device)
        self.channel = channel
        self.target_o = channel << 8
