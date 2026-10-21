import java.util.HashMap;
public class Maps {
 public static void main(String[] args) {
  if(args[0].equals("word_count")) {
   String[] words=new String[4096]; for(int i=0;i<words.length;i++) words[i]="token_"+i+"_"+(i*31);
   HashMap<String,Long> counts=new HashMap<>();
   System.out.println("@start work");
   for(int i=0;i<6000000;i++) {String w=words[i%4096]; long v=counts.getOrDefault(w,0L); counts.put(w,v+1);}
   System.out.println("@end work");
   long sum=0; for(String w:words) sum+=counts.get(w); System.out.println(sum);
  } else {
   HashMap<Long,Long> values=new HashMap<>(); long sum=0;
   System.out.println("@start work");
   for(long i=0;i<3000000;i++) values.put(i,i);
   for(long i=0;i<12000000;i++) sum+=values.get(i%3000000);
   System.out.println("@end work"); System.out.println(sum);
  }
 }
}
