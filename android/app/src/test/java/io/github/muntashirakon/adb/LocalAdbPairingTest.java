package io.github.muntashirakon.adb;

import org.junit.Test;
import java.io.ByteArrayInputStream;
import java.io.ByteArrayOutputStream;
import java.io.DataInputStream;
import java.io.DataOutputStream;
import java.io.IOException;
import static org.junit.Assert.assertArrayEquals;
import static org.junit.Assert.assertThrows;

public class LocalAdbPairingTest {
    private DataInputStream packet(int version, int type, int declaredSize, byte[] payload) throws IOException {
        ByteArrayOutputStream bytes = new ByteArrayOutputStream();
        DataOutputStream output = new DataOutputStream(bytes);
        output.writeByte(version);
        output.writeByte(type);
        output.writeInt(declaredSize);
        output.write(payload);
        return new DataInputStream(new ByteArrayInputStream(bytes.toByteArray()));
    }
    @Test public void readsCompletePayload() throws Exception {
        byte[] payload = new byte[] {1, 2, 3};
        assertArrayEquals(payload, LocalAdbPairing.readPacket(packet(1, 0, 3, payload), 0));
    }
    @Test public void rejectsWrongStageAndVersion() throws Exception {
        assertThrows(IOException.class, () -> LocalAdbPairing.readPacket(packet(2, 0, 1, new byte[] {1}), 0));
        assertThrows(IOException.class, () -> LocalAdbPairing.readPacket(packet(1, 1, 1, new byte[] {1}), 0));
    }
    @Test public void rejectsOversizedNegativeAndTruncatedPackets() throws Exception {
        for (int length : new int[] {-1, 0, Integer.MAX_VALUE, 16385, 2}) {
            assertThrows(IOException.class, () -> LocalAdbPairing.readPacket(packet(1, 0, length, new byte[] {1}), 0));
        }
    }
}
