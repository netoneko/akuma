// Same behaviour as the loop's hand-assembled Worker (denver/probes/mkworkload.py), compiled by a real javac.
public class Worker implements Runnable {
    public static int RESULT = 0;
    static volatile int A = 21, B = 2;   // multiplied at run time, so it is not constant-folded
    public void run() {
        System.out.println("WORKLOAD-THREAD-RAN");
        for (int i = 0; i < 200; i++) { byte[] b = new byte[64 * 1024]; b[0] = (byte) i; }
        RESULT = A * B;
    }
}
