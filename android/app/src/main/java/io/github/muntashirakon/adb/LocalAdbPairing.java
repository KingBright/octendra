package io.github.muntashirakon.adb;

import org.conscrypt.Conscrypt;

import java.io.Closeable;
import java.io.DataInputStream;
import java.io.DataOutputStream;
import java.io.IOException;
import java.net.InetSocketAddress;
import java.net.Socket;
import java.security.PrivateKey;
import java.security.cert.Certificate;
import java.security.interfaces.RSAPublicKey;
import java.util.Arrays;

import javax.net.ssl.SSLSocket;

/** Local-only, cancellable pairing transport for pinned LibADB 3.1.1.
 * Reuses its AOSP SPAKE2 implementation. The upstream pairing transport does
 * not expose its socket for cancellation during a TLS handshake.
 */
public final class LocalAdbPairing implements Closeable {
    private final Socket socket = new Socket();

    public void pair(int port, char[] code, PrivateKey key, Certificate certificate) throws Exception {
        if (port < 1 || port > 65535 || code.length != 6) {
            throw new IOException("invalid_pairing_input");
        }
        for (char digit : code) {
            if (digit < '0' || digit > '9') throw new IOException("invalid_pairing_input");
        }
        byte[] password = null;
        PairingAuthCtx auth = null;
        try {
            socket.connect(new InetSocketAddress("127.0.0.1", port), 5000);
            socket.setSoTimeout(10000);
            try (SSLSocket tls = (SSLSocket) SslUtils.getSslContext(new KeyPair(key, certificate))
                    .getSocketFactory().createSocket(socket, "127.0.0.1", port, true)) {
                tls.setSoTimeout(10000);
                tls.startHandshake();
                byte[] exported = Conscrypt.exportKeyingMaterial(tls, "adb-label\u0000", null, 64);
                password = new byte[code.length + exported.length];
                for (int i = 0; i < code.length; i++) password[i] = (byte) code[i];
                System.arraycopy(exported, 0, password, code.length, exported.length);
                Arrays.fill(exported, (byte) 0);
                auth = PairingAuthCtx.createAlice(password);
                if (auth == null) throw new IOException("pairing_crypto_unavailable");
                DataInputStream input = new DataInputStream(tls.getInputStream());
                DataOutputStream output = new DataOutputStream(tls.getOutputStream());
                writePacket(output, 0, auth.getMsg());
                if (!auth.initCipher(readPacket(input, 0))) throw new IOException("pairing_code_rejected");
                byte[] peer = new byte[8192];
                byte[] publicKey = AndroidPubkey.encodeWithName((RSAPublicKey) certificate.getPublicKey(), "Octendra");
                if (publicKey.length >= peer.length) throw new IOException("invalid_public_key_size");
                System.arraycopy(publicKey, 0, peer, 1, publicKey.length);
                byte[] encrypted = auth.encrypt(peer);
                if (encrypted == null) throw new IOException("pairing_encryption_failed");
                writePacket(output, 1, encrypted);
                byte[] response = auth.decrypt(readPacket(input, 1));
                if (response == null || response.length != 8192) throw new IOException("pairing_code_rejected");
            }
        } finally {
            Arrays.fill(code, '\0');
            if (password != null) Arrays.fill(password, (byte) 0);
            if (auth != null) auth.destroy();
            close();
        }
    }

    static byte[] readPacket(DataInputStream input, int expectedType) throws IOException {
        int version = input.readUnsignedByte();
        int type = input.readUnsignedByte();
        int size = input.readInt();
        if (version != 1 || type != expectedType || size < 1 || size > 16384) {
            throw new IOException("invalid_pairing_packet");
        }
        byte[] payload = new byte[size];
        input.readFully(payload);
        return payload;
    }

    private static void writePacket(DataOutputStream output, int type, byte[] payload) throws IOException {
        output.writeByte(1);
        output.writeByte(type);
        output.writeInt(payload.length);
        output.write(payload);
        output.flush();
    }

    @Override public void close() throws IOException {
        socket.close();
    }
}
