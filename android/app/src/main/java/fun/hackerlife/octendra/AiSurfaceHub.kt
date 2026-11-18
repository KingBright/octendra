package `fun`.hackerlife.octendra

import android.graphics.Bitmap
import android.graphics.SurfaceTexture
import android.opengl.EGL14
import android.opengl.EGLConfig
import android.opengl.EGLContext
import android.opengl.EGLDisplay
import android.opengl.EGLExt
import android.opengl.EGLSurface
import android.opengl.GLES11Ext
import android.opengl.GLES20
import android.os.Handler
import android.os.HandlerThread
import android.os.SystemClock
import android.view.Surface
import java.nio.ByteBuffer
import java.nio.ByteOrder
import java.nio.FloatBuffer
import java.util.UUID
import java.util.concurrent.FutureTask
import java.util.concurrent.TimeUnit

/**
 * Zero-CPU-copy OpenGL ES hub for an active background VirtualDisplay.
 *
 * Receives VirtualDisplay frames via an external OES SurfaceTexture and draws them directly
 * on the GPU to any consumer Surface registered via AiViewBridgeProvider.attach_video.
 * CPU readback (FBO + glReadPixels) occurs ONLY on explicit snapshot() calls for sparse AI ui_observe.
 */
internal class AiSurfaceHub(initialWidth: Int, initialHeight: Int) : AutoCloseable {
    private data class ConsumerOutput(
        val id: String,
        val surface: Surface,
        val eglSurface: EGLSurface
    )

    private val glThread = HandlerThread("octendra-ai-surface-hub").apply { start() }
    private val glHandler = Handler(glThread.looper)
    private val frameLock = java.lang.Object()
    private val consumers = LinkedHashMap<String, ConsumerOutput>()

    @Volatile var width: Int = initialWidth.coerceAtLeast(2); private set
    @Volatile var height: Int = initialHeight.coerceAtLeast(2); private set
    @Volatile var frameSequence: Long = 0L; private set
    @Volatile private var hasFrame = false
    @Volatile private var closed = false

    private var eglDisplay: EGLDisplay = EGL14.EGL_NO_DISPLAY
    private var eglConfig: EGLConfig? = null
    private var eglContext: EGLContext = EGL14.EGL_NO_CONTEXT
    private var pbufferSurface: EGLSurface = EGL14.EGL_NO_SURFACE

    private var program = 0
    private var aPositionLoc = -1
    private var aTexCoordLoc = -1
    private var uTexMatrixLoc = -1
    private var uTextureLoc = -1

    private var oesTexId = 0
    private var fboId = 0
    private var fboTexId = 0
    private var fboWidth = 0
    private var fboHeight = 0

    private var surfaceTexture: SurfaceTexture? = null
    private var producerSurface: Surface? = null
    private var lastTimestampNs: Long = 0L
    private val texMatrix = FloatArray(16)
    private var readBuffer: ByteBuffer? = null

    private val windowQuad: FloatBuffer = ByteBuffer.allocateDirect(16 * 4)
        .order(ByteOrder.nativeOrder()).asFloatBuffer().apply {
            put(floatArrayOf(
                -1f, -1f, 0f, 0f,
                1f, -1f, 1f, 0f,
                -1f, 1f, 0f, 1f,
                1f, 1f, 1f, 1f
            ))
            position(0)
        }

    // FBO glReadPixels reads row 0 from GL y=-1 first, whereas Bitmap.copyPixelsFromBuffer
    // treats row 0 as top-of-image. Flip V here so FBO readback produces an upright Bitmap directly.
    private val fboQuad: FloatBuffer = ByteBuffer.allocateDirect(16 * 4)
        .order(ByteOrder.nativeOrder()).asFloatBuffer().apply {
            put(floatArrayOf(
                -1f, -1f, 0f, 1f,
                1f, -1f, 1f, 1f,
                -1f, 1f, 0f, 0f,
                1f, 1f, 1f, 0f
            ))
            position(0)
        }

    val inputSurface: Surface
        get() = checkNotNull(producerSurface) { "ai_surface_hub_closed" }

    init {
        val initTask = FutureTask { initGl() }
        if (!glHandler.post(initTask)) throw IllegalStateException("ai_surface_hub_thread_unavailable")
        try {
            initTask.get(5, TimeUnit.SECONDS)
        } catch (e: Exception) {
            close()
            throw e
        }
    }

    private fun initGl() {
        eglDisplay = EGL14.eglGetDisplay(EGL14.EGL_DEFAULT_DISPLAY)
        check(eglDisplay != EGL14.EGL_NO_DISPLAY) { "eglGetDisplay failed" }
        val version = IntArray(2)
        check(EGL14.eglInitialize(eglDisplay, version, 0, version, 1)) { "eglInitialize failed" }

        eglConfig = chooseConfig(recordable = true) ?: chooseConfig(recordable = false)
            ?: throw IllegalStateException("No compatible EGLConfig")

        eglContext = EGL14.eglCreateContext(
            eglDisplay,
            eglConfig,
            EGL14.EGL_NO_CONTEXT,
            intArrayOf(EGL14.EGL_CONTEXT_CLIENT_VERSION, 2, EGL14.EGL_NONE),
            0
        )
        check(eglContext != EGL14.EGL_NO_CONTEXT) { "eglCreateContext failed" }

        pbufferSurface = EGL14.eglCreatePbufferSurface(
            eglDisplay,
            eglConfig,
            intArrayOf(EGL14.EGL_WIDTH, 1, EGL14.EGL_HEIGHT, 1, EGL14.EGL_NONE),
            0
        )
        check(pbufferSurface != EGL14.EGL_NO_SURFACE) { "eglCreatePbufferSurface failed" }
        check(EGL14.eglMakeCurrent(eglDisplay, pbufferSurface, pbufferSurface, eglContext)) { "eglMakeCurrent failed" }

        program = createProgram(VERTEX_SHADER, FRAGMENT_SHADER)
        aPositionLoc = GLES20.glGetAttribLocation(program, "aPosition")
        aTexCoordLoc = GLES20.glGetAttribLocation(program, "aTexCoord")
        uTexMatrixLoc = GLES20.glGetUniformLocation(program, "uTexMatrix")
        uTextureLoc = GLES20.glGetUniformLocation(program, "uTexture")

        val tex = IntArray(1)
        GLES20.glGenTextures(1, tex, 0)
        oesTexId = tex[0]
        GLES20.glBindTexture(GLES11Ext.GL_TEXTURE_EXTERNAL_OES, oesTexId)
        GLES20.glTexParameteri(GLES11Ext.GL_TEXTURE_EXTERNAL_OES, GLES20.GL_TEXTURE_MIN_FILTER, GLES20.GL_LINEAR)
        GLES20.glTexParameteri(GLES11Ext.GL_TEXTURE_EXTERNAL_OES, GLES20.GL_TEXTURE_MAG_FILTER, GLES20.GL_LINEAR)
        GLES20.glTexParameteri(GLES11Ext.GL_TEXTURE_EXTERNAL_OES, GLES20.GL_TEXTURE_WRAP_S, GLES20.GL_CLAMP_TO_EDGE)
        GLES20.glTexParameteri(GLES11Ext.GL_TEXTURE_EXTERNAL_OES, GLES20.GL_TEXTURE_WRAP_T, GLES20.GL_CLAMP_TO_EDGE)

        android.opengl.Matrix.setIdentityM(texMatrix, 0)
        val st = SurfaceTexture(oesTexId)
        st.setDefaultBufferSize(width, height)
        st.setOnFrameAvailableListener({ onFrameAvailable() }, glHandler)
        surfaceTexture = st
        producerSurface = Surface(st)
    }

    private fun chooseConfig(recordable: Boolean): EGLConfig? {
        val attrs = if (recordable) {
            intArrayOf(
                EGL14.EGL_RED_SIZE, 8,
                EGL14.EGL_GREEN_SIZE, 8,
                EGL14.EGL_BLUE_SIZE, 8,
                EGL14.EGL_ALPHA_SIZE, 8,
                EGL14.EGL_RENDERABLE_TYPE, EGL14.EGL_OPENGL_ES2_BIT,
                EGL14.EGL_SURFACE_TYPE, EGL14.EGL_WINDOW_BIT or EGL14.EGL_PBUFFER_BIT,
                EGL_RECORDABLE_ANDROID, 1,
                EGL14.EGL_NONE
            )
        } else {
            intArrayOf(
                EGL14.EGL_RED_SIZE, 8,
                EGL14.EGL_GREEN_SIZE, 8,
                EGL14.EGL_BLUE_SIZE, 8,
                EGL14.EGL_ALPHA_SIZE, 8,
                EGL14.EGL_RENDERABLE_TYPE, EGL14.EGL_OPENGL_ES2_BIT,
                EGL14.EGL_SURFACE_TYPE, EGL14.EGL_WINDOW_BIT or EGL14.EGL_PBUFFER_BIT,
                EGL14.EGL_NONE
            )
        }
        val configs = arrayOfNulls<EGLConfig>(1)
        val count = IntArray(1)
        return if (EGL14.eglChooseConfig(eglDisplay, attrs, 0, configs, 0, 1, count, 0) && count[0] > 0) {
            configs[0]
        } else null
    }

    private fun onFrameAvailable() {
        if (closed) return
        val st = surfaceTexture ?: return
        if (!EGL14.eglMakeCurrent(eglDisplay, pbufferSurface, pbufferSurface, eglContext)) return
        runCatching {
            st.updateTexImage()
            st.getTransformMatrix(texMatrix)
            lastTimestampNs = st.timestamp
            hasFrame = true
        }.onFailure { return }
        synchronized(frameLock) {
            frameSequence += 1L
            frameLock.notifyAll()
        }
        renderToAllConsumers()
        EGL14.eglMakeCurrent(eglDisplay, pbufferSurface, pbufferSurface, eglContext)
    }

    private fun renderToAllConsumers() {
        if (consumers.isEmpty()) return
        val list = consumers.values.toList()
        for (consumer in list) {
            if (!consumer.surface.isValid) {
                destroyConsumer(consumer.id)
                continue
            }
            if (!renderToConsumer(consumer)) {
                destroyConsumer(consumer.id)
            }
        }
    }

    private fun renderToConsumer(consumer: ConsumerOutput): Boolean {
        if (!EGL14.eglMakeCurrent(eglDisplay, consumer.eglSurface, consumer.eglSurface, eglContext)) return false
        val cw = IntArray(1)
        val ch = IntArray(1)
        if (!EGL14.eglQuerySurface(eglDisplay, consumer.eglSurface, EGL14.EGL_WIDTH, cw, 0) ||
            !EGL14.eglQuerySurface(eglDisplay, consumer.eglSurface, EGL14.EGL_HEIGHT, ch, 0)
        ) {
            return false
        }
        val viewW = cw[0].coerceAtLeast(1)
        val viewH = ch[0].coerceAtLeast(1)
        GLES20.glBindFramebuffer(GLES20.GL_FRAMEBUFFER, 0)
        GLES20.glViewport(0, 0, viewW, viewH)
        drawQuad(windowQuad)
        if (lastTimestampNs > 0L) {
            runCatching { EGLExt.eglPresentationTimeANDROID(eglDisplay, consumer.eglSurface, lastTimestampNs) }
        }
        return EGL14.eglSwapBuffers(eglDisplay, consumer.eglSurface)
    }

    private fun drawQuad(quad: FloatBuffer) {
        GLES20.glUseProgram(program)
        GLES20.glActiveTexture(GLES20.GL_TEXTURE0)
        GLES20.glBindTexture(GLES11Ext.GL_TEXTURE_EXTERNAL_OES, oesTexId)
        GLES20.glUniform1i(uTextureLoc, 0)
        GLES20.glUniformMatrix4fv(uTexMatrixLoc, 1, false, texMatrix, 0)

        quad.position(0)
        GLES20.glVertexAttribPointer(aPositionLoc, 2, GLES20.GL_FLOAT, false, 16, quad)
        GLES20.glEnableVertexAttribArray(aPositionLoc)
        quad.position(2)
        GLES20.glVertexAttribPointer(aTexCoordLoc, 2, GLES20.GL_FLOAT, false, 16, quad)
        GLES20.glEnableVertexAttribArray(aTexCoordLoc)

        GLES20.glDrawArrays(GLES20.GL_TRIANGLE_STRIP, 0, 4)

        GLES20.glDisableVertexAttribArray(aPositionLoc)
        GLES20.glDisableVertexAttribArray(aTexCoordLoc)
    }

    fun attachConsumer(surface: Surface): String {
        require(!closed) { "ai_surface_hub_closed" }
        require(surface.isValid) { "invalid_consumer_surface" }
        val task = FutureTask<String> {
            check(!closed) { "ai_surface_hub_closed" }
            check(surface.isValid) { "invalid_consumer_surface" }
            val eglSurface = EGL14.eglCreateWindowSurface(eglDisplay, eglConfig, surface, intArrayOf(EGL14.EGL_NONE), 0)
            check(eglSurface != EGL14.EGL_NO_SURFACE) { "eglCreateWindowSurface_failed" }
            val id = UUID.randomUUID().toString()
            val consumer = ConsumerOutput(id, surface, eglSurface)
            consumers[id] = consumer
            if (hasFrame) {
                if (!renderToConsumer(consumer)) {
                    destroyConsumer(id)
                    throw IllegalStateException("consumer_initial_render_failed")
                }
                EGL14.eglMakeCurrent(eglDisplay, pbufferSurface, pbufferSurface, eglContext)
            }
            id
        }
        check(glHandler.post(task)) { "ai_surface_hub_closed" }
        return task.get(5, TimeUnit.SECONDS)
    }

    fun detachConsumer(consumerId: String) {
        if (closed) return
        val task = FutureTask {
            destroyConsumer(consumerId)
            if (eglDisplay != EGL14.EGL_NO_DISPLAY && pbufferSurface != EGL14.EGL_NO_SURFACE && eglContext != EGL14.EGL_NO_CONTEXT) {
                EGL14.eglMakeCurrent(eglDisplay, pbufferSurface, pbufferSurface, eglContext)
            }
        }
        if (glHandler.post(task)) {
            runCatching { task.get(3, TimeUnit.SECONDS) }
        }
    }

    private fun destroyConsumer(consumerId: String) {
        val removed = consumers.remove(consumerId) ?: return
        if (eglDisplay != EGL14.EGL_NO_DISPLAY) {
            if (EGL14.eglGetCurrentSurface(EGL14.EGL_DRAW) == removed.eglSurface) {
                EGL14.eglMakeCurrent(eglDisplay, pbufferSurface, pbufferSurface, eglContext)
            }
            EGL14.eglDestroySurface(eglDisplay, removed.eglSurface)
        }
    }

    fun updateSize(newWidth: Int, newHeight: Int) {
        if (closed || newWidth < 2 || newHeight < 2) return
        if (width == newWidth && height == newHeight) return
        val task = FutureTask {
            if (closed) return@FutureTask
            width = newWidth
            height = newHeight
            surfaceTexture?.setDefaultBufferSize(newWidth, newHeight)
        }
        if (glHandler.post(task)) {
            runCatching { task.get(3, TimeUnit.SECONDS) }
        }
    }

    fun awaitFrameAfter(previous: Long, timeoutMs: Long): Boolean = synchronized(frameLock) {
        val end = SystemClock.elapsedRealtime() + timeoutMs
        while (frameSequence <= previous && !closed && SystemClock.elapsedRealtime() < end) {
            val left = (end - SystemClock.elapsedRealtime()).coerceAtLeast(1L)
            frameLock.wait(minOf(80L, left))
        }
        frameSequence > previous
    }

    /** Explicit CPU readback via offscreen FBO + glReadPixels; only called for sparse AI ui_observe. */
    fun snapshotBitmap(waitMs: Long = 750L): Bitmap? {
        synchronized(frameLock) {
            val end = SystemClock.elapsedRealtime() + waitMs
            while (!hasFrame && !closed && SystemClock.elapsedRealtime() < end) {
                val left = (end - SystemClock.elapsedRealtime()).coerceAtLeast(1L)
                frameLock.wait(minOf(80L, left))
            }
            if (!hasFrame || closed) return null
        }
        val task = FutureTask<Bitmap?> {
            if (closed || !hasFrame) return@FutureTask null
            if (!EGL14.eglMakeCurrent(eglDisplay, pbufferSurface, pbufferSurface, eglContext)) return@FutureTask null
            val w = width
            val h = height
            ensureFbo(w, h)
            GLES20.glBindFramebuffer(GLES20.GL_FRAMEBUFFER, fboId)
            GLES20.glViewport(0, 0, w, h)
            drawQuad(fboQuad)
            GLES20.glFinish()
            val needed = w * h * 4
            val buf = readBuffer?.takeIf { it.capacity() >= needed }
                ?: ByteBuffer.allocateDirect(needed).order(ByteOrder.nativeOrder()).also { readBuffer = it }
            buf.clear()
            buf.limit(needed)
            GLES20.glReadPixels(0, 0, w, h, GLES20.GL_RGBA, GLES20.GL_UNSIGNED_BYTE, buf)
            GLES20.glBindFramebuffer(GLES20.GL_FRAMEBUFFER, 0)
            buf.rewind()
            val bitmap = Bitmap.createBitmap(w, h, Bitmap.Config.ARGB_8888)
            bitmap.copyPixelsFromBuffer(buf)
            bitmap
        }
        if (!glHandler.post(task)) return null
        return runCatching { task.get(5, TimeUnit.SECONDS) }.getOrNull()
    }

    private fun ensureFbo(w: Int, h: Int) {
        if (fboId != 0 && fboTexId != 0 && fboWidth == w && fboHeight == h) return
        releaseFbo()
        val fbos = IntArray(1)
        val texs = IntArray(1)
        GLES20.glGenFramebuffers(1, fbos, 0)
        GLES20.glGenTextures(1, texs, 0)
        fboId = fbos[0]
        fboTexId = texs[0]
        fboWidth = w
        fboHeight = h

        GLES20.glBindTexture(GLES20.GL_TEXTURE_2D, fboTexId)
        GLES20.glTexParameteri(GLES20.GL_TEXTURE_2D, GLES20.GL_TEXTURE_MIN_FILTER, GLES20.GL_LINEAR)
        GLES20.glTexParameteri(GLES20.GL_TEXTURE_2D, GLES20.GL_TEXTURE_MAG_FILTER, GLES20.GL_LINEAR)
        GLES20.glTexParameteri(GLES20.GL_TEXTURE_2D, GLES20.GL_TEXTURE_WRAP_S, GLES20.GL_CLAMP_TO_EDGE)
        GLES20.glTexParameteri(GLES20.GL_TEXTURE_2D, GLES20.GL_TEXTURE_WRAP_T, GLES20.GL_CLAMP_TO_EDGE)
        GLES20.glTexImage2D(GLES20.GL_TEXTURE_2D, 0, GLES20.GL_RGBA, w, h, 0, GLES20.GL_RGBA, GLES20.GL_UNSIGNED_BYTE, null)

        GLES20.glBindFramebuffer(GLES20.GL_FRAMEBUFFER, fboId)
        GLES20.glFramebufferTexture2D(GLES20.GL_FRAMEBUFFER, GLES20.GL_COLOR_ATTACHMENT0, GLES20.GL_TEXTURE_2D, fboTexId, 0)
        check(GLES20.glCheckFramebufferStatus(GLES20.GL_FRAMEBUFFER) == GLES20.GL_FRAMEBUFFER_COMPLETE) {
            "FBO incomplete"
        }
        GLES20.glBindFramebuffer(GLES20.GL_FRAMEBUFFER, 0)
    }

    private fun releaseFbo() {
        if (fboId != 0) {
            GLES20.glDeleteFramebuffers(1, intArrayOf(fboId), 0)
            fboId = 0
        }
        if (fboTexId != 0) {
            GLES20.glDeleteTextures(1, intArrayOf(fboTexId), 0)
            fboTexId = 0
        }
        fboWidth = 0
        fboHeight = 0
    }

    override fun close() {
        if (closed) return
        closed = true
        synchronized(frameLock) { frameLock.notifyAll() }
        val cleanup = FutureTask {
            for (id in consumers.keys.toList()) destroyConsumer(id)
            if (eglDisplay != EGL14.EGL_NO_DISPLAY && eglContext != EGL14.EGL_NO_CONTEXT) {
                if (pbufferSurface != EGL14.EGL_NO_SURFACE) {
                    EGL14.eglMakeCurrent(eglDisplay, pbufferSurface, pbufferSurface, eglContext)
                }
                releaseFbo()
                if (oesTexId != 0) {
                    GLES20.glDeleteTextures(1, intArrayOf(oesTexId), 0)
                    oesTexId = 0
                }
                if (program != 0) {
                    GLES20.glDeleteProgram(program)
                    program = 0
                }
            }
            runCatching { producerSurface?.release() }; producerSurface = null
            runCatching { surfaceTexture?.release() }; surfaceTexture = null
            if (eglDisplay != EGL14.EGL_NO_DISPLAY) {
                EGL14.eglMakeCurrent(eglDisplay, EGL14.EGL_NO_SURFACE, EGL14.EGL_NO_SURFACE, EGL14.EGL_NO_CONTEXT)
                if (pbufferSurface != EGL14.EGL_NO_SURFACE) {
                    EGL14.eglDestroySurface(eglDisplay, pbufferSurface)
                    pbufferSurface = EGL14.EGL_NO_SURFACE
                }
                if (eglContext != EGL14.EGL_NO_CONTEXT) {
                    EGL14.eglDestroyContext(eglDisplay, eglContext)
                    eglContext = EGL14.EGL_NO_CONTEXT
                }
                EGL14.eglTerminate(eglDisplay)
                eglDisplay = EGL14.EGL_NO_DISPLAY
            }
            readBuffer = null
        }
        if (glHandler.post(cleanup)) {
            runCatching { cleanup.get(3, TimeUnit.SECONDS) }
        }
        glThread.quitSafely()
    }

    private fun createProgram(vertexSrc: String, fragmentSrc: String): Int {
        val vs = compileShader(GLES20.GL_VERTEX_SHADER, vertexSrc)
        val fs = compileShader(GLES20.GL_FRAGMENT_SHADER, fragmentSrc)
        val prog = GLES20.glCreateProgram()
        GLES20.glAttachShader(prog, vs)
        GLES20.glAttachShader(prog, fs)
        GLES20.glLinkProgram(prog)
        GLES20.glDeleteShader(vs)
        GLES20.glDeleteShader(fs)
        val status = IntArray(1)
        GLES20.glGetProgramiv(prog, GLES20.GL_LINK_STATUS, status, 0)
        check(status[0] != 0) { "GL program link failed: ${GLES20.glGetProgramInfoLog(prog)}" }
        return prog
    }

    private fun compileShader(type: Int, src: String): Int {
        val shader = GLES20.glCreateShader(type)
        GLES20.glShaderSource(shader, src)
        GLES20.glCompileShader(shader)
        val status = IntArray(1)
        GLES20.glGetShaderiv(shader, GLES20.GL_COMPILE_STATUS, status, 0)
        check(status[0] != 0) { "GL shader compile failed: ${GLES20.glGetShaderInfoLog(shader)}" }
        return shader
    }

    companion object {
        private const val EGL_RECORDABLE_ANDROID = 0x3142

        private const val VERTEX_SHADER = """
            attribute vec4 aPosition;
            attribute vec4 aTexCoord;
            uniform mat4 uTexMatrix;
            varying vec2 vTexCoord;
            void main() {
                vTexCoord = (uTexMatrix * aTexCoord).xy;
                gl_Position = aPosition;
            }
        """

        private const val FRAGMENT_SHADER = """
            #extension GL_OES_EGL_image_external : require
            precision mediump float;
            varying vec2 vTexCoord;
            uniform samplerExternalOES uTexture;
            void main() {
                gl_FragColor = texture2D(uTexture, vTexCoord);
            }
        """
    }
}
